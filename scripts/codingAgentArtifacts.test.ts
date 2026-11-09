import { execFileSync, spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const verifier = path.join(repoRoot, "scripts/verify-coding-agent-artifacts.mjs");
const buildScript = path.join(repoRoot, "scripts/build-coding-agent-executor.sh");
const manifestSchema = path.join(repoRoot, "resources/coding-agent/artifact-manifest.schema.json");
const expectedFeatures = [
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
];
const minimumFeatures = ["prepare"];
const legacyFeatures = expectedFeatures.filter((feature) => !["resolve_base", "check_repository", "inspect_source", "push_workspace", "lookup_publication"].includes(feature));
const configPaths = [
  "workers/lookback-coding-agent-workers.yaml",
  "workers/workflows/coding-agent/prepare.yaml",
  "workers/workflows/coding-agent/one-shot.yaml",
  "workers/workflows/coding-agent/publish.yaml",
];
const fixtureGitEnv = {
  PATH: process.env.PATH ?? "/usr/bin:/bin",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_TERMINAL_PROMPT: "0",
};

let scratchRoot: string;
let fixtureSequence = 0;

beforeEach(() => {
  fs.mkdirSync("/tmp/opencode", { recursive: true });
  scratchRoot = fs.mkdtempSync("/tmp/opencode/coding-agent-artifacts-");
  fixtureSequence = 0;
});

afterEach(() => {
  fs.rmSync(scratchRoot, { recursive: true, force: true });
});

function git(repo: string, ...args: string[]): string {
  return execFileSync(
    "git",
    ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C", repo, ...args],
    { encoding: "utf8", env: fixtureGitEnv },
  ).trim();
}

function fixtureRepo(root: string, name: string, remote: string) {
  const repo = path.join(root, name);
  fs.mkdirSync(repo);
  execFileSync("git", ["init", "--quiet", "--initial-branch", `reviewed-${name}`, repo], {
    env: fixtureGitEnv,
  });
  git(repo, "remote", "add", "origin", remote);
  git(repo, "config", "user.name", "artifact fixture");
  git(repo, "config", "user.email", "artifact-fixture@example.invalid");
  fs.writeFileSync(path.join(repo, "source.txt"), name);
  git(repo, "add", "source.txt");
  git(repo, "commit", "--quiet", "-m", "fixture source");
  return {
    repository: remote,
    ref: `reviewed-${name}`,
    revision: git(repo, "rev-parse", "HEAD"),
  };
}

function localGit(repo: string, ...args: string[]): string {
  return execFileSync("git", ["-C", repo, ...args], {
    encoding: "utf8",
    env: fixtureGitEnv,
  }).trim();
}

function initLocalRepo(root: string, name: string) {
  const repository = path.join(root, name);
  fs.mkdirSync(repository, { recursive: true });
  const branch = `reviewed-${name.replace(/[^A-Za-z0-9-]/g, "-")}`;
  execFileSync("git", ["init", "--quiet", "--initial-branch", branch, repository], {
    env: fixtureGitEnv,
  });
  localGit(repository, "config", "user.name", "artifact fixture");
  localGit(repository, "config", "user.email", "artifact-fixture@example.invalid");
  return repository;
}

function commitFixture(repository: string, relative: string, contents: string) {
  const file = path.join(repository, relative);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, contents);
  localGit(repository, "add", "--", relative);
  localGit(repository, "commit", "--quiet", "-m", `add ${relative}`);
  return localGit(repository, "rev-parse", "HEAD");
}

function makeBuildFixture() {
  const fixtureRoot = path.join(scratchRoot, `build-fixture-${fixtureSequence++}`);
  fs.mkdirSync(fixtureRoot);

  const leaf = initLocalRepo(fixtureRoot, "leaf source");
  const leafRevision = commitFixture(leaf, "leaf.txt", "leaf fixture\n");
  const parent = initLocalRepo(fixtureRoot, "parent source");
  commitFixture(parent, "parent.txt", "parent fixture\n");
  execFileSync(
    "git",
    ["-c", "protocol.file.allow=always", "-C", parent, "submodule", "add", leaf, "nested child"],
    { env: fixtureGitEnv },
  );
  localGit(parent, "commit", "--quiet", "-am", "add nested child");
  const parentRevision = localGit(parent, "rev-parse", "HEAD");

  const app = initLocalRepo(fixtureRoot, "app source");
  localGit(app, "remote", "add", "origin", "https://example.invalid/lookback/agent-app.git");
  const client = initLocalRepo(fixtureRoot, "client source");
  const clientRevision = commitFixture(client, "client.txt", "client fixture\n");
  const lockClientUrl = `https://example.invalid/jobworkerp/client.git?branch=reviewed-client-source#${clientRevision}`;
  const appFiles = [
    ["Cargo.lock", `version = 4\n\n[[package]]\nname = "jobworkerp-client"\nversion = "0.1.0"\nsource = "git+${lockClientUrl}"\n`],
    ["coding-agent-executor/Cargo.toml", '[package]\nname = "coding-agent-executor"\nversion = "0.1.0"\n'],
    ["coding-agent-executor/src/model.rs", "pub const PROTOCOL_VERSION: u32 = 2;\n"],
    ...configPaths.map((relative, index) => [relative, `fixture config ${index}\n`]),
  ] as const;
  for (const [relative, contents] of appFiles) {
    const file = path.join(app, relative);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, contents);
    localGit(app, "add", "--", relative);
  }
  const appScripts = path.join(app, "scripts");
  fs.mkdirSync(appScripts, { recursive: true });
  for (const relative of [
    "scripts/build-coding-agent-executor.sh",
    "scripts/verify-coding-agent-artifacts.mjs",
    "scripts/lib/coding-agent-manifest.mjs",
    "scripts/lib/git-origin.mjs",
  ]) {
    const copied = path.join(app, relative);
    fs.mkdirSync(path.dirname(copied), { recursive: true });
    fs.copyFileSync(path.join(repoRoot, relative), copied);
  }
  fs.chmodSync(path.join(app, "scripts/build-coding-agent-executor.sh"), 0o755);
  localGit(app, "add", "scripts");
  localGit(app, "commit", "--quiet", "-m", "app build fixture");

  execFileSync(
    "git",
    ["-c", "protocol.file.allow=always", "-C", app, "submodule", "add", parent, "modules/parent child"],
    { env: fixtureGitEnv },
  );
  execFileSync(
    "git",
    ["-c", "protocol.file.allow=always", "-C", app, "submodule", "update", "--init", "--recursive"],
    { env: fixtureGitEnv },
  );
  localGit(app, "add", ".gitmodules", "modules/parent child");
  localGit(app, "commit", "--quiet", "-m", "add parent submodule");

  const runtime = initLocalRepo(fixtureRoot, "runtime source");
  localGit(runtime, "remote", "add", "origin", "https://example.invalid/jobworkerp/runtime.git");
  commitFixture(runtime, "Cargo.toml", "[workspace]\nresolver = \"3\"\n");

  const fakeBin = path.join(fixtureRoot, "fake-bin");
  fs.mkdirSync(fakeBin);
  const cargo = path.join(fakeBin, "cargo");
  fs.writeFileSync(
    cargo,
    `#!/bin/sh
set -eu
target=
package=
while [ $# -gt 0 ]; do
  case "$1" in
    --target) target=$2; shift 2 ;;
    -p) package=$2; shift 2 ;;
    *) shift ;;
  esac
done
case "$package" in
  jobworkerp-main) binary=all-in-one ;;
  coding-agent-executor) binary=coding-agent-executor ;;
  *) exit 90 ;;
esac
out="$CARGO_TARGET_DIR/$target/release/$binary"
mkdir -p "$(dirname "$out")"
if [ "$binary" = coding-agent-executor ]; then
  cat > "$out" <<'EXECUTOR'
#!/bin/sh
case "$1" in
  --version) printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=2' ;;
  --capabilities) printf '%s\\n' '{"protocol_version":2,"executor_version":"0.1.0","features":${JSON.stringify(expectedFeatures)}}' ;;
  *) exit 97 ;;
esac
EXECUTOR
else
  printf 'fake fixture artifact: %s\\n' "$binary" > "$out"
fi
chmod 755 "$out"
`,
    { mode: 0o755 },
  );
  fs.chmodSync(cargo, 0o755);

  const pins = {
    LOOKBACK_CODING_AGENT_APP_REPO_URL: "https://example.invalid/lookback/agent-app.git",
    LOOKBACK_CODING_AGENT_APP_REF: "reviewed-app-source",
    LOOKBACK_CODING_AGENT_APP_REVISION: localGit(app, "rev-parse", "HEAD"),
    LOOKBACK_CODING_AGENT_RUNTIME_URL: "https://example.invalid/jobworkerp/runtime.git",
    LOOKBACK_CODING_AGENT_RUNTIME_REF: "reviewed-runtime-source",
    LOOKBACK_CODING_AGENT_RUNTIME_REVISION: localGit(runtime, "rev-parse", "HEAD"),
    LOOKBACK_CODING_AGENT_CLIENT_REVISION: clientRevision,
  };
  return {
    app,
    runtime,
    leafRevision,
    parentRevision,
    fakeBin,
    output: path.join(fixtureRoot, "output artifacts"),
    pins,
    rustupHome: path.join(fixtureRoot, "rustup"),
    cargoHome: path.join(fixtureRoot, "cargo-home"),
  };
}

function buildFixture(fixture: ReturnType<typeof makeBuildFixture>) {
  fs.mkdirSync(fixture.rustupHome);
  fs.mkdirSync(fixture.cargoHome);
  return spawnSync(
    "bash",
    [
      path.join(fixture.app, "scripts/build-coding-agent-executor.sh"),
      "--target",
      "x86_64-unknown-linux-gnu",
      "--runtime-source",
      fixture.runtime,
      "--artifact-dir",
      fixture.output,
    ],
    {
      cwd: fixture.app,
      encoding: "utf8",
      timeout: 15_000,
      env: {
        PATH: `${fixture.fakeBin}:/usr/bin:/bin`,
        RUSTUP_HOME: fixture.rustupHome,
        LOOKBACK_CODING_AGENT_CARGO_HOME: fixture.cargoHome,
        ...fixture.pins,
      },
    },
  );
}

function sha256(file: string): string {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function makeFixture() {
  const fixtureRoot = path.join(scratchRoot, `fixture-${fixtureSequence++}`);
  fs.mkdirSync(fixtureRoot);
  const app = fixtureRepo(fixtureRoot, "app", "https://example.invalid/lookback/agent-app.git");
  const runtime = fixtureRepo(
    fixtureRoot,
    "runtime",
    "https://example.invalid/jobworkerp/jobworkerp-rs.git",
  );
  const client = fixtureRepo(
    fixtureRoot,
    "client",
    "https://example.invalid/jobworkerp/jobworkerp-client-rs.git",
  );
  const artifactRoot = path.join(fixtureRoot, "artifacts");
  const configRoot = path.join(fixtureRoot, "configuration");
  fs.mkdirSync(path.join(artifactRoot, "runtime"), { recursive: true });
  fs.mkdirSync(path.join(artifactRoot, "bin"), { recursive: true });
  fs.mkdirSync(configRoot, { recursive: true });
  const clientLock = [
    "version = 4",
    "",
    "[[package]]",
    'name = "jobworkerp-client"',
    'version = "0.1.0"',
    `source = "git+${client.repository}?branch=${client.ref}#${client.revision}"`,
    "",
  ].join("\n");
  fs.writeFileSync(path.join(configRoot, "Cargo.lock"), clientLock);

  const runtimeBinary = path.join(artifactRoot, "runtime/all-in-one");
  fs.writeFileSync(runtimeBinary, "fixture-runtime-bytes");
  const executorBinary = path.join(artifactRoot, "bin/coding-agent-executor");
  fs.writeFileSync(
    executorBinary,
    `#!/bin/sh\ncase "$1" in\n  --version) printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=2' ;;\n  --capabilities) printf '%s\\n' '{"protocol_version":2,"executor_version":"0.1.0","features":${JSON.stringify(expectedFeatures)}}' ;;\n  *) exit 97 ;;\nesac\n`,
    { mode: 0o755 },
  );

  const configuration = configPaths.map((relative, index) => {
    const file = path.join(configRoot, relative);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, `fixture config ${index}\n`);
    return { path: relative, sha256: sha256(file) };
  });

  const manifestPath = path.join(artifactRoot, "coding-agent-artifact-manifest.json");
  const manifest = {
    schema_version: 1,
    profile: { name: "linux-kvm", version: 1 },
    target: { triple: "x86_64-unknown-linux-gnu" },
    sources: {
      agent_app: { ...app, dirty: false, submodules: [] },
      jobworkerp_runtime: { ...runtime, dirty: false, submodules: [] },
      jobworkerp_client: {
        repository: client.repository,
        revision: client.revision,
        lockfile_sha256: sha256(path.join(configRoot, "Cargo.lock")),
      },
    },
    executor: { version: "0.1.0", protocol_version: 2, capabilities: [...expectedFeatures] },
    runtime_compatibility: {
      protocol_version: null,
      config_version: null,
    },
    artifacts: {
      runtime: { path: "runtime/all-in-one", sha256: sha256(runtimeBinary) },
      executor: { path: "bin/coding-agent-executor", sha256: sha256(executorBinary) },
      configuration,
    },
  };
  fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  return { artifactRoot, configRoot, manifest, manifestPath, runtimeBinary, executorBinary };
}

function setFixtureCapabilities(fixture: ReturnType<typeof makeFixture>, features: string[]) {
  fixture.manifest.executor.capabilities = [...features];
  setFixtureHelperCapabilities(fixture, features);
  fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
}

function setFixtureHelperCapabilities(fixture: ReturnType<typeof makeFixture>, features: string[]) {
  fs.writeFileSync(
    fixture.executorBinary,
    `#!/bin/sh\ncase "$1" in\n  --version) printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=2' ;;\n  --capabilities) printf '%s\\n' '{"protocol_version":2,"executor_version":"0.1.0","features":${JSON.stringify(features)}}' ;;\n  *) exit 97 ;;\nesac\n`,
    { mode: 0o755 },
  );
  fixture.manifest.artifacts.executor.sha256 = sha256(fixture.executorBinary);
}

function verify(
  fixture: ReturnType<typeof makeFixture>,
  options: {
    provenanceOnly?: boolean;
    env?: Record<string, string | undefined>;
    timeoutMs?: number;
  } = {},
) {
  const args = [
    verifier,
    "--manifest",
    fixture.manifestPath,
    "--artifact-root",
    fixture.artifactRoot,
    "--config-root",
    fixture.configRoot,
    "--json",
  ];
  if (options.provenanceOnly) args.push("--provenance-only");
  const result = spawnSync("node", args, {
    cwd: repoRoot,
    encoding: "utf8",
    env: { ...fixtureGitEnv, ...options.env },
    timeout: options.timeoutMs ?? 8_000,
    killSignal: "SIGKILL",
    maxBuffer: 1024 * 1024,
  });
  const report = result.stdout.trim() ? JSON.parse(result.stdout) : undefined;
  return { ...result, report };
}

function terminateRecordedProcess(pidFile: string) {
  if (!fs.existsSync(pidFile)) return;
  const pid = Number(fs.readFileSync(pidFile, "utf8").trim());
  if (!Number.isSafeInteger(pid) || pid < 2) return;
  try {
    process.kill(pid, "SIGKILL");
  } catch {
    // The bounded verifier probe may already have killed and reaped it.
  }
}

function recordedProcessIsStopped(pidFile: string): boolean {
  if (!fs.existsSync(pidFile)) return false;
  const pid = Number(fs.readFileSync(pidFile, "utf8").trim());
  if (!Number.isSafeInteger(pid) || pid < 2) return false;
  const result = spawnSync("ps", ["-o", "stat=", "-p", String(pid)], {
    encoding: "utf8",
    timeout: 1_000,
    killSignal: "SIGKILL",
  });
  return result.status !== 0 || result.stdout.trim().startsWith("Z");
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function closeWithin(child: ReturnType<typeof spawn>, timeoutMs: number) {
  return new Promise<{ code: number | null; signal: string | null } | undefined>((resolve) => {
    const timer = setTimeout(() => resolve(undefined), timeoutMs);
    child.once("close", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal });
    });
  });
}

describe("coding-agent artifact verification", () => {
  it("maps the native package directory to the resource path selected by bootstrap", () => {
    const config = JSON.parse(fs.readFileSync(path.join(repoRoot, "src-tauri/tauri.conf.json"), "utf8"));
    expect(config.bundle.resources["../resources/coding-agent/"]).toBe("coding-agent/");
    expect(fs.statSync(path.join(repoRoot, "resources/coding-agent")).isDirectory()).toBe(true);
  });

  it("declares resolve_base as optional while keeping the core capability contract required", () => {
    const schema = JSON.parse(fs.readFileSync(manifestSchema, "utf8"));
    const capabilitiesSchema = schema.properties.executor.properties.capabilities;
    const requiredCapabilities = capabilitiesSchema.allOf.flatMap((rule: { contains?: { const?: string } }) =>
      typeof rule.contains?.const === "string" ? [rule.contains.const] : [],
    );

    expect(capabilitiesSchema.minItems).toBe(minimumFeatures.length);
    expect(capabilitiesSchema.maxItems).toBe(expectedFeatures.length);
    expect(capabilitiesSchema.items.enum).toContain("resolve_base");
    expect(capabilitiesSchema.items.enum).toContain("check_repository");
    expect(capabilitiesSchema.items.enum).toContain("inspect_source");
    expect(requiredCapabilities).toEqual(minimumFeatures);
  });

  it("builds a local fake artifact from clean initialized nested submodules and records their pins", () => {
    const fixture = makeBuildFixture();
    const result = buildFixture(fixture);

    expect(result.status, `${result.stdout}${result.stderr}`).toBe(0);
    expect(fs.readFileSync(path.join(fixture.output, "runtime/all-in-one"), "utf8")).toContain(
      "fake fixture artifact: all-in-one",
    );
    expect(fs.readFileSync(path.join(fixture.output, "bin/coding-agent-executor"), "utf8")).toContain(
      "coding-agent-executor 0.1.0 protocol=2",
    );
    const manifest = JSON.parse(
      fs.readFileSync(path.join(fixture.output, "coding-agent-artifact-manifest.json"), "utf8"),
    );
    expect(manifest.executor.protocol_version).toBe(2);
    expect(manifest.executor.capabilities).toEqual(expectedFeatures);
    expect(manifest.runtime_compatibility).not.toHaveProperty("quota_capability");
    expect(manifest.sources.agent_app.submodules).toEqual([
      { path: "modules/parent child", revision: fixture.parentRevision, dirty: false },
      {
        path: "modules/parent child/nested child",
        revision: fixture.leafRevision,
        dirty: false,
      },
    ]);
  });

  it("rejects a nested source edit hidden by local submodule ignore settings before fake compilation", () => {
    const fixture = makeBuildFixture();
    localGit(fixture.app, "config", "submodule.modules/parent child.ignore", "all");
    fs.writeFileSync(
      path.join(fixture.app, "modules/parent child/nested child/leaf.txt"),
      "dirty nested submodule source\n",
    );

    const result = buildFixture(fixture);
    expect(result.status).not.toBe(0);
    expect(`${result.stdout}${result.stderr}`).toContain("source tree is dirty");
    expect(fs.existsSync(fixture.output)).toBe(false);
  });

  it("builds dynamically registered workflows from a pinned source without worker YAML", () => {
    const fixture = makeBuildFixture();
    localGit(fixture.app, "rm", "--", ...configPaths);
    localGit(fixture.app, "commit", "--quiet", "-m", "remove unused workflow files");
    fixture.pins.LOOKBACK_CODING_AGENT_APP_REVISION = localGit(fixture.app, "rev-parse", "HEAD");
    const result = buildFixture(fixture);
    expect(result.status, `${result.stdout}${result.stderr}`).toBe(0);
    const manifest = JSON.parse(fs.readFileSync(path.join(fixture.output, "coding-agent-artifact-manifest.json"), "utf8"));
    expect(manifest.artifacts.configuration).toEqual([]);
  });

  it.each(["source", '"source"', "'source'", '"so\\u0075rce"'])("rejects duplicate %s lock fields before publishing an artifact", (key) => {
    const fixture = makeBuildFixture();
    const lockPath = path.join(fixture.app, "Cargo.lock");
    const lockText = fs.readFileSync(lockPath, "utf8");
    const sourceLine = lockText.match(/^source = .*$/m)?.[0];
    if (!sourceLine) throw new Error("fixture client source line is missing");
    const duplicate = sourceLine.replace(/^source/, key);
    fs.writeFileSync(lockPath, lockText.replace(sourceLine, `${sourceLine}\n${duplicate}`));
    localGit(fixture.app, "add", "Cargo.lock");
    localGit(fixture.app, "commit", "--quiet", "-m", "add duplicate lock source");
    fixture.pins.LOOKBACK_CODING_AGENT_APP_REVISION = localGit(fixture.app, "rev-parse", "HEAD");

    const result = buildFixture(fixture);
    expect(result.status).not.toBe(0);
    expect(`${result.stdout}${result.stderr}`).toContain("could not record coding-agent artifact provenance");
    expect(fs.existsSync(fixture.output)).toBe(false);
  });

  it.each(['[["package"]]', "[['package']]", '[["pack\\u0061ge"]]'])("rejects hidden duplicate package header %s in the build input", (header) => {
    const fixture = makeBuildFixture();
    const lockPath = path.join(fixture.app, "Cargo.lock");
    const lockText = fs.readFileSync(lockPath, "utf8");
    const hidden = `${header}\nname = "jobworkerp-client"\nsource = "git+https://example.invalid/other-client.git#${"e".repeat(40)}"\n`;
    fs.writeFileSync(lockPath, `${hidden}\n${lockText}`);
    localGit(fixture.app, "add", "Cargo.lock");
    localGit(fixture.app, "commit", "--quiet", "-m", "add hidden duplicate package");
    fixture.pins.LOOKBACK_CODING_AGENT_APP_REVISION = localGit(fixture.app, "rev-parse", "HEAD");
    const result = buildFixture(fixture);
    expect(result.status).not.toBe(0);
    expect(fs.existsSync(fixture.output)).toBe(false);
  });

  it("validates pinned source and artifact hashes but keeps feature enablement fail-closed", () => {
    const fixture = makeFixture();
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status, result.stderr).toBe(0);
    expect(result.report).toMatchObject({
      provenance_valid: true,
      enable_ready: false,
      diagnostics: [
        "installed_runtime_protocol_compatibility_unverified",
        "installed_runtime_config_compatibility_unverified",
      ],
    });
    expect(fixture.manifest.runtime_compatibility).not.toHaveProperty("quota_capability");
    expect(result.report.executor).toEqual({
      version: "0.1.0",
      protocol_version: 2,
      capabilities: expectedFeatures,
    });
  });

  it("does not treat valid provenance as permission to enable an unverified runtime", () => {
    const result = verify(makeFixture());

    expect(result.status).not.toBe(0);
    expect(result.report).toMatchObject({ provenance_valid: true, enable_ready: false });
  });

  it("rejects an artifact whose bytes no longer match the manifest", () => {
    const fixture = makeFixture();
    fs.writeFileSync(fixture.runtimeBinary, "runtime replaced after manifest creation");
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toContain("artifact_hash_mismatch");
  });

  it("rejects a protocol-v1 helper with a fixed version diagnostic", () => {
    const fixture = makeFixture();
    fs.writeFileSync(
      fixture.executorBinary,
      `#!/bin/sh\ncase "$1" in\n  --version) printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=1' ;;\n  --capabilities) exit 0 ;;\n  *) exit 97 ;;\nesac\n`,
      { mode: 0o755 },
    );
    fixture.manifest.artifacts.executor.sha256 = sha256(fixture.executorBinary);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_version_mismatch"]);
    expect(result.report.enable_ready).toBe(false);
  });

  it("accepts a prepare-only v2 helper without artifact or publication capabilities", () => {
    const fixture = makeFixture();
    const features = [...minimumFeatures];
    setFixtureCapabilities(fixture, features);

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status, result.stderr).toBe(0);
    expect(result.report).toMatchObject({ provenance_valid: true, enable_ready: false });
    expect(result.report.executor.capabilities).toEqual(minimumFeatures);
  });

  it("verifies the optional workspace publication handshake without enabling execution", () => {
    const fixture = makeFixture();
    setFixtureCapabilities(fixture, expectedFeatures);
    const result = verify(fixture, { provenanceOnly: true });
    expect(result.status, result.stderr).toBe(0);
    expect(result.report).toMatchObject({ provenance_valid: true, enable_ready: false });
    expect(result.report.executor.capabilities).toContain("push_workspace");
  });

  it("accepts the legacy eight-capability handshake without resolve_base", () => {
    const fixture = makeFixture();
    setFixtureCapabilities(fixture, legacyFeatures);

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status, result.stderr).toBe(0);
    expect(result.report).toMatchObject({ provenance_valid: true, enable_ready: false });
    expect(result.report.executor.capabilities).toEqual(legacyFeatures);
  });

  it("accepts resolve_base as an optional v2 capability without enabling execution", () => {
    const fixture = makeFixture();
    setFixtureCapabilities(fixture, [...minimumFeatures, "resolve_base"]);

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status, result.stderr).toBe(0);
    expect(result.report).toMatchObject({ provenance_valid: true, enable_ready: false });
    expect(result.report.executor.capabilities).toEqual([...minimumFeatures, "resolve_base"]);
  });

  it("rejects unknown manifest capabilities instead of silently ignoring future features", () => {
    const fixture = makeFixture();
    setFixtureCapabilities(fixture, [...minimumFeatures, "future_capability"]);

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_capabilities_unsupported"]);
  });

  it("rejects duplicate capabilities in the helper handshake", () => {
    const fixture = makeFixture();
    setFixtureHelperCapabilities(fixture, [...expectedFeatures, "prepare"]);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_capabilities_mismatch"]);
  });

  it("rejects a capability manifest without workspace preparation", () => {
    const fixture = makeFixture();
    setFixtureCapabilities(fixture, ["capture", "seal", "resolve", "artifact_only_publication"]);

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_capabilities_unsupported"]);
  });

  it.each(minimumFeatures)("rejects a v2 manifest/helper missing required %s", (missing) => {
    const fixture = makeFixture();
    const features = expectedFeatures.filter((feature) => feature !== missing);
    setFixtureCapabilities(fixture, features);

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_capabilities_unsupported"]);
  });

  it("rejects a helper advertising more capabilities than a valid manifest", () => {
    const fixture = makeFixture();
    fixture.manifest.executor.capabilities = [...minimumFeatures];
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_capabilities_mismatch"]);
  });

  it("rejects a protocol-v1 manifest with a fixed unsupported-protocol diagnostic", () => {
    const fixture = makeFixture();
    fixture.manifest.executor.protocol_version = 1;
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));

    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_protocol_unsupported"]);
  });

  it("rejects helper capabilities that differ from the manifest", () => {
    const fixture = makeFixture();
    const reportedFeatures = expectedFeatures.slice(0, -1);
    fs.writeFileSync(
      fixture.executorBinary,
      `#!/bin/sh\ncase "$1" in\n  --version) printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=2' ;;\n  --capabilities) printf '%s\\n' '{"protocol_version":2,"executor_version":"0.1.0","features":${JSON.stringify(reportedFeatures)}}' ;;\n  *) exit 97 ;;\nesac\n`,
      { mode: 0o755 },
    );
    fixture.manifest.artifacts.executor.sha256 = sha256(fixture.executorBinary);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toContain("executor_capabilities_mismatch");
  });

  it("bounds helper probes, kills a TERM-ignoring process group, and returns only a fixed diagnostic", () => {
    const fixture = makeFixture();
    const pidFile = path.join(scratchRoot, "probe-child.pid");
    fs.writeFileSync(
      fixture.executorBinary,
      `#!/bin/sh
case "$1" in
  --version)
    sh -c 'printf "%s\\n" "$$" > "$1"; trap "" TERM; while :; do sleep 1; done' sh ${shellQuote(pidFile)} &
    trap '' TERM
    wait ;;
  *) exit 97 ;;
esac
`,
      { mode: 0o755 },
    );
    fixture.manifest.artifacts.executor.sha256 = sha256(fixture.executorBinary);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));

    const startedAt = performance.now();
    let result: ReturnType<typeof verify> | undefined;
    let childWasStopped = false;
    try {
      result = verify(fixture, { provenanceOnly: true, timeoutMs: 6_500 });
      childWasStopped = recordedProcessIsStopped(pidFile);
    } finally {
      terminateRecordedProcess(pidFile);
    }
    const duration = performance.now() - startedAt;

    if (!result) throw new Error("verifier did not return a result");
    expect(duration).toBeLessThan(5_000);
    expect(childWasStopped).toBe(true);
    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toEqual(["executor_probe_failed"]);
    expect(result.stdout.length).toBeLessThan(2_000);
    expect(result.stderr).not.toContain(pidFile);
  }, 7_000);

  it("cleans up active helper process groups when the verifier receives SIGINT", async () => {
    const fixture = makeFixture();
    const pidFile = path.join(scratchRoot, "sigint-probe-child.pid");
    fs.writeFileSync(
      fixture.executorBinary,
      `#!/bin/sh
case "$1" in
  --version)
    sh -c 'printf "%s\\n" "$$" > "$1"; trap "" TERM; while :; do sleep 1; done' sh ${shellQuote(pidFile)} &
    trap '' TERM
    wait ;;
  *) exit 97 ;;
esac
`,
      { mode: 0o755 },
    );
    fixture.manifest.artifacts.executor.sha256 = sha256(fixture.executorBinary);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const child = spawn(
      "node",
      [
        verifier,
        "--manifest",
        fixture.manifestPath,
        "--artifact-root",
        fixture.artifactRoot,
        "--config-root",
        fixture.configRoot,
        "--provenance-only",
        "--json",
      ],
      { cwd: repoRoot, env: { PATH: fixtureGitEnv.PATH }, stdio: ["ignore", "pipe", "ignore"] },
    );
    let stdout = "";
    const output = child.stdout;
    if (!output) throw new Error("verifier stdout pipe was not created");
    output.setEncoding("utf8");
    output.on("data", (chunk: string) => {
      if (stdout.length < 2_000) stdout += chunk;
    });

    try {
      const pidDeadline = performance.now() + 1_000;
      while (!fs.existsSync(pidFile) && performance.now() < pidDeadline) {
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      expect(fs.existsSync(pidFile)).toBe(true);
      child.kill("SIGINT");
      const result = await closeWithin(child, 1_500);
      expect(result).toMatchObject({ code: 130, signal: null });
      expect(JSON.parse(stdout).diagnostics).toEqual(["executor_probe_failed"]);
      expect(recordedProcessIsStopped(pidFile)).toBe(true);
    } finally {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      terminateRecordedProcess(pidFile);
    }
  }, 4_000);

  it("caps probe output and gives helper binaries a minimal environment", () => {
    const environment = makeFixture();
    fs.writeFileSync(
      environment.executorBinary,
      `#!/bin/sh
case "$1" in
  --version)
    [ -z "${"$"}{CODING_AGENT_TEST_SECRET:-}" ] || exit 91
    printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=2' ;;
  --capabilities) printf '%s\\n' '{"protocol_version":2,"executor_version":"0.1.0","features":${JSON.stringify(expectedFeatures)}}' ;;
  *) exit 97 ;;
esac
`,
      { mode: 0o755 },
    );
    environment.manifest.artifacts.executor.sha256 = sha256(environment.executorBinary);
    fs.writeFileSync(environment.manifestPath, JSON.stringify(environment.manifest));
    expect(
      verify(environment, {
        provenanceOnly: true,
        env: { CODING_AGENT_TEST_SECRET: "do-not-pass-to-helper" },
      }).status,
    ).toBe(0);

    const overflow = makeFixture();
    fs.writeFileSync(
      overflow.executorBinary,
      "#!/bin/sh\ncase \"$1\" in\n  --version) printf '%070000d\\n' 0 ;;\n  *) exit 0 ;;\nesac\n",
      { mode: 0o755 },
    );
    overflow.manifest.artifacts.executor.sha256 = sha256(overflow.executorBinary);
    fs.writeFileSync(overflow.manifestPath, JSON.stringify(overflow.manifest));
    const result = verify(overflow, { provenanceOnly: true });
    expect(result.report.diagnostics).toContain("executor_probe_failed");
    expect(result.stdout.length).toBeLessThan(2_000);
  });

  it("accepts dynamically registered workflows without packaged YAML and rejects duplicate optional assets", () => {
    const fixture = makeFixture();
    fixture.manifest.artifacts.configuration = [];
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    expect(verify(fixture, { provenanceOnly: true }).status).toBe(0);
    const duplicate = makeFixture();
    duplicate.manifest.artifacts.configuration.push(duplicate.manifest.artifacts.configuration[0]);
    fs.writeFileSync(duplicate.manifestPath, JSON.stringify(duplicate.manifest));
    expect(verify(duplicate, { provenanceOnly: true }).report.diagnostics).toContain("manifest_invalid");
  });

  it("rejects configuration drift and a helper capability claim outside the reviewed contract", () => {
    const changedConfig = makeFixture();
    fs.writeFileSync(path.join(changedConfig.configRoot, configPaths[0]), "changed config\n");
    expect(verify(changedConfig, { provenanceOnly: true }).report.diagnostics).toContain(
      "configuration_hash_mismatch",
    );

    const unsupportedCapability = makeFixture();
    unsupportedCapability.manifest.executor.capabilities.push("quota_reservation");
    fs.writeFileSync(unsupportedCapability.manifestPath, JSON.stringify(unsupportedCapability.manifest));
    expect(verify(unsupportedCapability, { provenanceOnly: true }).report.diagnostics).toContain(
      "executor_capabilities_unsupported",
    );

    const pushWithoutScan = makeFixture();
    pushWithoutScan.manifest.executor.capabilities = expectedFeatures.filter(
      (feature) => feature !== "secret_scan_supported_formats_v1",
    );
    fs.writeFileSync(pushWithoutScan.manifestPath, JSON.stringify(pushWithoutScan.manifest));
    expect(verify(pushWithoutScan, { provenanceOnly: true }).report.diagnostics).toContain(
      "executor_capabilities_unsupported",
    );

    const pullRequestWithoutPush = makeFixture();
    pullRequestWithoutPush.manifest.executor.capabilities = expectedFeatures.filter(
      (feature) => feature !== "push" && feature !== "secret_scan_supported_formats_v1",
    );
    fs.writeFileSync(pullRequestWithoutPush.manifestPath, JSON.stringify(pullRequestWithoutPush.manifest));
    expect(verify(pullRequestWithoutPush, { provenanceOnly: true }).report.diagnostics).toContain(
      "executor_capabilities_unsupported",
    );
  });

  it("checks arbitrary optional assets and rejects excess configuration entries", () => {
    const fixture = makeFixture();
    const relative = "optional/settings.json";
    const file = path.join(fixture.configRoot, relative);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, "optional asset\n");
    fixture.manifest.artifacts.configuration = [{ path: relative, sha256: sha256(file) }];
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    expect(verify(fixture, { provenanceOnly: true }).status).toBe(0);
    fs.writeFileSync(file, "changed\n");
    expect(verify(fixture, { provenanceOnly: true }).report.diagnostics).toContain("configuration_hash_mismatch");
    fs.rmSync(file);
    expect(verify(fixture, { provenanceOnly: true }).report.diagnostics).toContain("configuration_missing");
    const boundary = makeFixture();
    boundary.manifest.artifacts.configuration = Array.from({ length: 256 }, (_, index) => {
      const relative = `optional-${index}.json`;
      const file = path.join(boundary.configRoot, relative);
      fs.writeFileSync(file, "asset\n");
      return { path: relative, sha256: sha256(file) };
    });
    fs.writeFileSync(boundary.manifestPath, JSON.stringify(boundary.manifest));
    expect(verify(boundary, { provenanceOnly: true }).status).toBe(0);
    boundary.manifest.artifacts.configuration.push({ path: "excess.json", sha256: "a".repeat(64) });
    fs.writeFileSync(boundary.manifestPath, JSON.stringify(boundary.manifest));
    expect(verify(boundary, { provenanceOnly: true }).report.diagnostics).toContain("manifest_invalid");
  });

  it("ties the client/proto pin to the lockfile used by the build", () => {
    const fixture = makeFixture();
    fixture.manifest.sources.jobworkerp_client.revision = "c".repeat(40);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toContain("client_lock_mismatch");
  });

  it("rejects duplicate client source fields and package records but accepts formatted CRLF lock records", () => {
    const replaceLock = (fixture: ReturnType<typeof makeFixture>, lockText: string) => {
      fs.writeFileSync(path.join(fixture.configRoot, "Cargo.lock"), lockText);
      fixture.manifest.sources.jobworkerp_client.lockfile_sha256 = sha256(
        path.join(fixture.configRoot, "Cargo.lock"),
      );
      fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
      return verify(fixture, { provenanceOnly: true });
    };
    const readSourceLine = (fixture: ReturnType<typeof makeFixture>) => {
      const lockText = fs.readFileSync(path.join(fixture.configRoot, "Cargo.lock"), "utf8");
      const line = lockText.match(/^source = .*$/m)?.[0];
      if (!line) throw new Error("fixture client source line is missing");
      return { lockText, line };
    };

    const duplicateSourceFixture = makeFixture();
    const identical = readSourceLine(duplicateSourceFixture);
    const duplicateIdentical = replaceLock(
      duplicateSourceFixture,
      identical.lockText.replace(identical.line, `${identical.line}\n${identical.line}`),
    );
    expect(duplicateIdentical.report.diagnostics).toContain("client_lock_invalid");

    for (const key of ['"source"', "'source'", '"so\\u0075rce"']) {
      for (const changed of [false, true]) {
        const fixture = makeFixture();
        const source = readSourceLine(fixture);
        const duplicate = changed
          ? `${key} = "git+https://example.invalid/other-client.git#${"e".repeat(40)}"`
          : source.line.replace(/^source/, key);
        const result = replaceLock(fixture, source.lockText.replace(source.line, `${source.line}\n${duplicate}`));
        expect(result.report.diagnostics, `${key} changed=${changed}`).toContain("client_lock_invalid");
      }
    }

    const duplicateWrong = makeFixture();
    const wrongLock = readSourceLine(duplicateWrong);
    const wrongSource = `source = "git+https://example.invalid/unexpected/client.git#${"e".repeat(40)}"`;
    const duplicateWrongResult = replaceLock(
      duplicateWrong,
      wrongLock.lockText.replace(wrongLock.line, `${wrongLock.line}\n${wrongSource}`),
    );
    expect(duplicateWrongResult.report.diagnostics).toContain("client_lock_invalid");

    const duplicatePackage = makeFixture();
    const packageLock = readSourceLine(duplicatePackage);
    const duplicatePackageResult = replaceLock(
      duplicatePackage,
      `${packageLock.lockText}\n[[package]]\nname = "jobworkerp-client"\nversion = "0.1.0"\n${packageLock.line}\n`,
    );
    expect(duplicatePackageResult.report.diagnostics).toContain("client_lock_invalid");

    for (const header of ['[["package"]]', "[['package']]", '[["pack\\u0061ge"]]']) {
      const fixture = makeFixture();
      const source = readSourceLine(fixture);
      const hidden = `${header}\nname = "jobworkerp-client"\nsource = "git+https://example.invalid/other-client.git#${"e".repeat(40)}"\n`;
      const result = replaceLock(fixture, `${hidden}\n${source.lockText}`);
      expect(result.report.diagnostics, header).toContain("client_lock_invalid");
    }

    const formatted = makeFixture();
    const formattedLock = readSourceLine(formatted);
    const formattedText = fs
      .readFileSync(path.join(formatted.configRoot, "Cargo.lock"), "utf8")
      .replace('name = "jobworkerp-client"', '  name   = "jobworkerp-client" # canonical package')
      .replace(formattedLock.line, `${formattedLock.line} # locked source`)
      .replaceAll("\n", "\r\n");
    const formattedResult = replaceLock(formatted, formattedText);
    expect(formattedResult.report, JSON.stringify(formattedResult.report)).toMatchObject({
      provenance_valid: true,
    });
  });

  it("keeps runtime compatibility limited to unverified protocol/config versions", () => {
    const schema = JSON.parse(fs.readFileSync(manifestSchema, "utf8"));
    expect(schema.properties.executor.properties.protocol_version).toEqual({ const: 2 });
    expect(schema.properties.executor.properties.capabilities.items.enum).toEqual(expectedFeatures);
    expect(schema.properties.executor.properties.capabilities.minItems).toBe(minimumFeatures.length);
    expect(schema.properties.executor.properties.capabilities.maxItems).toBe(expectedFeatures.length);
    expect(schema.properties.runtime_compatibility.required).toEqual([
      "protocol_version",
      "config_version",
    ]);
    expect(schema.properties.runtime_compatibility.properties.protocol_version).toEqual({
      type: "null",
    });
    expect(schema.properties.runtime_compatibility.properties).not.toHaveProperty("quota_capability");
    expect(schema.properties.runtime_compatibility.additionalProperties).toBe(false);
    expect(schema.$defs.submodule.properties.path.pattern).toContain(" ");
  });

  it("rejects oversized Cargo.lock inputs before parsing", () => {
    const fixture = makeFixture();
    const lockPath = path.join(fixture.configRoot, "Cargo.lock");
    fs.writeFileSync(lockPath, Buffer.alloc(8 * 1024 * 1024 + 1, 0x41));
    fixture.manifest.sources.jobworkerp_client.lockfile_sha256 = sha256(lockPath);
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));

    const result = verify(fixture, { provenanceOnly: true });
    expect(result.report.diagnostics).toContain("client_lock_invalid");
  });

  it("rejects missing runtime artifacts and configuration files", () => {
    const missingArtifact = makeFixture();
    fs.rmSync(missingArtifact.runtimeBinary);
    const artifactResult = verify(missingArtifact, { provenanceOnly: true });
    expect(artifactResult.status).not.toBe(0);
    expect(artifactResult.report.diagnostics).toContain("artifact_missing");

    const missingConfig = makeFixture();
    fs.rmSync(path.join(missingConfig.configRoot, configPaths[0]));
    const configResult = verify(missingConfig, { provenanceOnly: true });
    expect(configResult.status).not.toBe(0);
    expect(configResult.report.diagnostics).toContain("configuration_missing");
  });

  it("rejects unknown fields, unsupported targets, and profile version drift", () => {
    const unknownField = makeFixture();
    Object.assign(unknownField.manifest, { unreviewed: true });
    fs.writeFileSync(unknownField.manifestPath, JSON.stringify(unknownField.manifest));
    expect(verify(unknownField, { provenanceOnly: true }).report.diagnostics).toContain(
      "manifest_invalid",
    );

    const target = makeFixture();
    target.manifest.target.triple = "aarch64-apple-darwin";
    fs.writeFileSync(target.manifestPath, JSON.stringify(target.manifest));
    expect(verify(target, { provenanceOnly: true }).report.diagnostics).toContain(
      "unsupported_target",
    );

    const profile = makeFixture();
    profile.manifest.profile.version = 2;
    fs.writeFileSync(profile.manifestPath, JSON.stringify(profile.manifest));
    expect(verify(profile, { provenanceOnly: true }).report.diagnostics).toContain(
      "unsupported_profile_version",
    );
  });

  it("rejects credential-bearing source URLs without echoing their contents", () => {
    const fixture = makeFixture();
    fixture.manifest.sources.jobworkerp_runtime.repository =
      "https://user:do-not-print@example.invalid/runtime.git";
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toContain("manifest_invalid");
    expect(result.stdout).not.toContain("do-not-print");
    expect(result.stderr).not.toContain("do-not-print");
  });

  it("rejects traversal and symlink artifact paths", () => {
    const traversal = makeFixture();
    traversal.manifest.artifacts.runtime.path = "../outside";
    fs.writeFileSync(traversal.manifestPath, JSON.stringify(traversal.manifest));
    expect(verify(traversal, { provenanceOnly: true }).report.diagnostics).toContain(
      "manifest_invalid",
    );

    const symlinked = makeFixture();
    const link = path.join(symlinked.artifactRoot, "bin/executor-copy");
    fs.copyFileSync(symlinked.executorBinary, link);
    fs.rmSync(symlinked.executorBinary);
    fs.symlinkSync(link, symlinked.executorBinary);
    fs.writeFileSync(symlinked.manifestPath, JSON.stringify(symlinked.manifest));
    expect(verify(symlinked, { provenanceOnly: true }).report.diagnostics).toContain(
      "artifact_unsafe_path",
    );
  });

  it("refuses paths outside the current checkout and approved scratch tree", () => {
    const result = spawnSync(
      "node",
      [
        verifier,
        "--manifest",
        "/home/not-approved/coding-agent-artifact-manifest.json",
        "--artifact-root",
        "/home/not-approved",
        "--config-root",
        "/home/not-approved",
        "--json",
      ],
      { cwd: repoRoot, encoding: "utf8", env: { PATH: fixtureGitEnv.PATH } },
    );

    expect(result.status).not.toBe(0);
    expect(JSON.parse(result.stdout).diagnostics).toContain("artifact_unsafe_path");
  });

  it("rejects a legacy quota field as an unknown manifest property", () => {
    const fixture = makeFixture();
    Object.assign(fixture.manifest.runtime_compatibility, { quota_capability: "verified" });
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toContain("manifest_invalid");
    expect(result.report.enable_ready).toBe(false);
  });

  it("keeps protocol and config compatibility fail-closed without a runtime attestation", () => {
    const fixture = makeFixture();
    fixture.manifest.runtime_compatibility = { protocol_version: 1, config_version: "1" };
    fs.writeFileSync(fixture.manifestPath, JSON.stringify(fixture.manifest));
    const result = verify(fixture, { provenanceOnly: true });

    expect(result.status).not.toBe(0);
    expect(result.report.diagnostics).toContain("runtime_compatibility_unverified");
    expect(result.report.enable_ready).toBe(false);
  });

  it("requires explicit source pins in coding-agent build mode before side effects", () => {
    const result = spawnSync(
      "bash",
      [buildScript, "--target", "x86_64-unknown-linux-gnu", "--runtime-source", scratchRoot],
      { cwd: repoRoot, encoding: "utf8", env: { PATH: fixtureGitEnv.PATH } },
    );

    expect(result.status).not.toBe(0);
    expect(`${result.stdout}${result.stderr}`).toContain("explicit source URL, ref, and full SHA");
    expect(fs.readdirSync(scratchRoot)).toEqual([]);
  });

  it("keeps the legacy runtime default while requiring pins in the opt-in dependency build", () => {
    const script = `
      source '${path.join(repoRoot, "scripts/lib/build-common.sh")}';
      AGENT_APP='${repoRoot}';
      source '${path.join(repoRoot, "scripts/lib/build-deps.sh")}';
      printf 'default-ref=%s\\n' "$(repo_branch jobworkerp)";
      LOOKBACK_CODING_AGENT_BUILD=1;
      SKIP_CLONE=1;
      ONLY_REPOS=jobworkerp;
      PLATFORM=linux;
      TRIPLE=x86_64-unknown-linux-gnu;
      validate_coding_agent_build_config;
    `;
    const result = spawnSync("bash", ["-c", script], {
      cwd: repoRoot,
      encoding: "utf8",
      env: { PATH: fixtureGitEnv.PATH },
    });

    expect(result.status).not.toBe(0);
    expect(result.stdout).toContain("default-ref=main");
    expect(`${result.stdout}${result.stderr}`).toContain("explicit source URL, ref, and full SHA");
  });

  it("accepts only a clean local runtime checkout matching its explicit pin", () => {
    const fixtureRoot = path.join(scratchRoot, "pinned-source-checks");
    fs.mkdirSync(fixtureRoot);
    const runtime = fixtureRepo(
      fixtureRoot,
      "runtime",
      "https://example.invalid/jobworkerp/jobworkerp-rs.git",
    );
    const runtimeRepository = path.join(fixtureRoot, "runtime");
    const childRepository = initLocalRepo(fixtureRoot, "runtime module");
    commitFixture(childRepository, "tracked.txt", "clean child source\n");
    const nestedRepository = initLocalRepo(fixtureRoot, "nested runtime module");
    commitFixture(nestedRepository, "nested.txt", "clean nested source\n");
    execFileSync(
      "git",
      [
        "-c",
        "protocol.file.allow=always",
        "-C",
        childRepository,
        "submodule",
        "add",
        "--name",
        "nested-module",
        nestedRepository,
        "nested child",
      ],
      { env: fixtureGitEnv },
    );
    localGit(childRepository, "add", ".gitmodules", "nested child");
    localGit(childRepository, "commit", "--quiet", "-m", "add nested child");
    execFileSync(
      "git",
      [
        "-c",
        "protocol.file.allow=always",
        "-C",
        runtimeRepository,
        "submodule",
        "add",
        "--name",
        "hidden-module",
        childRepository,
        "module with spaces",
      ],
      { env: fixtureGitEnv },
    );
    const childCheckout = path.join(runtimeRepository, "module with spaces");
    execFileSync(
      "git",
      [
        "-c",
        "protocol.file.allow=always",
        "-C",
        runtimeRepository,
        "submodule",
        "update",
        "--init",
        "--recursive",
      ],
      { env: fixtureGitEnv },
    );
    localGit(childCheckout, "config", "submodule.nested-module.ignore", "all");
    localGit(runtimeRepository, "add", ".gitmodules", "module with spaces");
    localGit(runtimeRepository, "commit", "--quiet", "-m", "add child module");
    localGit(runtimeRepository, "config", "submodule.hidden-module.ignore", "all");
    const runtimeRevision = localGit(runtimeRepository, "rev-parse", "HEAD");
    const script = `
      source '${path.join(repoRoot, "scripts/lib/build-common.sh")}';
      AGENT_APP='${repoRoot}';
      source '${path.join(repoRoot, "scripts/lib/build-deps.sh")}';
      validate_coding_agent_build_config;
      verify_pinned_jobworkerp_checkout "$RUNTIME_DIR" "$LOOKBACK_CODING_AGENT_RUNTIME_REF" \
        "$LOOKBACK_CODING_AGENT_RUNTIME_REVISION" "$LOOKBACK_CODING_AGENT_RUNTIME_URL";
    `;
    const runCheck = (ref = runtime.ref, revision = runtimeRevision) =>
      spawnSync("bash", ["-c", script], {
        cwd: repoRoot,
        encoding: "utf8",
        env: {
          PATH: fixtureGitEnv.PATH,
          AGENT_APP: repoRoot,
          RUNTIME_DIR: path.join(fixtureRoot, "runtime"),
          WORKDIR: fixtureRoot,
          LOOKBACK_CODING_AGENT_BUILD: "1",
          SKIP_CLONE: "1",
          ONLY_REPOS: "jobworkerp",
          PLATFORM: "linux",
          TRIPLE: "x86_64-unknown-linux-gnu",
          LOOKBACK_CODING_AGENT_RUNTIME_URL: runtime.repository,
          LOOKBACK_CODING_AGENT_RUNTIME_REF: ref,
          LOOKBACK_CODING_AGENT_RUNTIME_REVISION: revision,
          LOOKBACK_CODING_AGENT_APP_REPO_URL: "https://example.invalid/lookback/agent-app.git",
          LOOKBACK_CODING_AGENT_APP_REF: "reviewed-app",
          LOOKBACK_CODING_AGENT_APP_REVISION: "a".repeat(40),
          LOOKBACK_CODING_AGENT_CLIENT_REVISION: "b".repeat(40),
        },
      });

    expect(runCheck().status).toBe(0);
    const floating = runCheck("main");
    expect(`${floating.stdout}${floating.stderr}`).toContain("floating source refs are not accepted");
    expect(runCheck(runtime.ref, "c".repeat(40)).stderr).toContain(
      "HEAD does not match the configured full SHA",
    );

    fs.writeFileSync(
      path.join(runtimeRepository, "module with spaces/tracked.txt"),
      "dirty child source\n",
    );
    expect(runCheck().stderr).toContain("source tree is dirty");
    fs.writeFileSync(
      path.join(runtimeRepository, "module with spaces/tracked.txt"),
      "clean child source\n",
    );
    fs.writeFileSync(path.join(runtimeRepository, "module with spaces/untracked.txt"), "untracked\n");
    expect(runCheck().stderr).toContain("source tree is dirty");
    fs.rmSync(path.join(runtimeRepository, "module with spaces/untracked.txt"));
    localGit(childCheckout, "config", "user.name", "artifact fixture");
    localGit(childCheckout, "config", "user.email", "artifact-fixture@example.invalid");
    fs.writeFileSync(
      path.join(childCheckout, "nested child/nested.txt"),
      "dirty nested source\n",
    );
    expect(runCheck().stderr).toContain("source tree is dirty");
    fs.writeFileSync(
      path.join(childCheckout, "nested child/nested.txt"),
      "clean nested source\n",
    );
    commitFixture(childCheckout, "second.txt", "new child commit\n");
    expect(runCheck().stderr).toContain("source tree is dirty");
  });
});
