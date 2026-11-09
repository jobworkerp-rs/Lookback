#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";

const MAX_MANIFEST_BYTES = 1024 * 1024;
const MAX_PROBE_OUTPUT_BYTES = 64 * 1024;
const PROBE_TIMEOUT_MS = 3000;
const MANIFEST_NAME = "coding-agent-artifact-manifest.json";
const SUPPORTED_TARGETS = new Set(["x86_64-unknown-linux-gnu"]);
const SUPPORTED_PROTOCOL_VERSION = 2;
const REQUIRED_CAPABILITIES = ["prepare"];
const OPTIONAL_CAPABILITIES = [
  "capture",
  "seal",
  "resolve",
  "artifact_only_publication",
  "resolve_base",
  "check_repository",
  "inspect_source",
  "push",
  "push_workspace",
  "lookup_publication",
  "pull_request",
  "secret_scan_supported_formats_v1",
];
const SUPPORTED_CAPABILITIES = [...REQUIRED_CAPABILITIES, ...OPTIONAL_CAPABILITIES];
const activeProbeCleanups = new Set();
let terminationSignal;

function handleProbeSignal(signal) {
  terminationSignal = signal;
  for (const cleanup of [...activeProbeCleanups]) cleanup();
}

function registerProbeCleanup(cleanup) {
  if (activeProbeCleanups.size === 0) {
    process.on("SIGINT", handleSigint);
    process.on("SIGTERM", handleSigterm);
  }
  activeProbeCleanups.add(cleanup);
}

function unregisterProbeCleanup(cleanup) {
  activeProbeCleanups.delete(cleanup);
  if (activeProbeCleanups.size === 0) {
    process.off("SIGINT", handleSigint);
    process.off("SIGTERM", handleSigterm);
  }
}

function handleSigint() {
  handleProbeSignal("SIGINT");
}

function handleSigterm() {
  handleProbeSignal("SIGTERM");
}

class VerifyError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}

function parseArguments(argv) {
  const options = { json: false, provenanceOnly: false, allowDevelopmentSources: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--json") {
      options.json = true;
    } else if (argument === "--provenance-only") {
      options.provenanceOnly = true;
    } else if (argument === "--allow-development-sources") {
      // Development staging records dirty worktrees and branch heads; only a
      // debug build of the app accepts such a package, so the flag is never
      // used by release verification.
      options.allowDevelopmentSources = true;
    } else if (["--manifest", "--artifact-root", "--config-root"].includes(argument)) {
      const value = argv[index + 1];
      if (!value || value.startsWith("--")) throw new VerifyError("invalid_arguments");
      options[argument.slice(2).replaceAll("-", "")] = value;
      index += 1;
    } else {
      throw new VerifyError("invalid_arguments");
    }
  }
  if (!options.manifest || !options.artifactroot || !options.configroot) {
    throw new VerifyError("invalid_arguments");
  }
  return options;
}

function isWithin(parent, candidate) {
  const relative = path.relative(parent, candidate);
  return relative === "" || (!relative.startsWith(`..${path.sep}`) && relative !== ".." && !path.isAbsolute(relative));
}

function allowedDevelopmentPath(candidate) {
  const workingDirectory = fs.realpathSync(process.cwd());
  const scratchDirectory = "/tmp/opencode";
  return isWithin(workingDirectory, candidate) || isWithin(scratchDirectory, candidate);
}

function canonicalDirectory(input) {
  const absolute = path.resolve(input);
  const workingDirectory = fs.realpathSync(process.cwd());
  if (!isWithin(workingDirectory, absolute) && !isWithin("/tmp/opencode", absolute)) {
    throw new VerifyError("artifact_unsafe_path");
  }
  const allowedRoots = [workingDirectory, "/tmp/opencode"].filter((root) => isWithin(root, absolute));
  const base = allowedRoots.sort((left, right) => right.length - left.length)[0];
  let current = base;
  const relative = path.relative(base, absolute);
  try {
    const baseMetadata = fs.lstatSync(base);
    if (!baseMetadata.isDirectory() || baseMetadata.isSymbolicLink()) {
      throw new VerifyError("artifact_unsafe_path");
    }
    for (const component of relative ? relative.split(path.sep) : []) {
      current = path.join(current, component);
      const metadata = fs.lstatSync(current);
      if (metadata.isSymbolicLink()) throw new VerifyError("artifact_unsafe_path");
      if (current !== absolute && !metadata.isDirectory()) throw new VerifyError("artifact_unsafe_path");
    }
    const metadata = fs.lstatSync(current);
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) throw new VerifyError("artifact_unsafe_path");
  } catch (error) {
    if (error instanceof VerifyError) throw error;
    if (error?.code === "ENOENT") throw new VerifyError("artifact_missing");
    throw error;
  }
  const canonical = fs.realpathSync(absolute);
  if (!allowedDevelopmentPath(canonical)) throw new VerifyError("artifact_unsafe_path");
  return canonical;
}

function safeRelativePath(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= 512 &&
    !path.isAbsolute(value) &&
    !value.includes("\\") &&
    value.split("/").every((part) => part.length > 0 && part !== "." && part !== "..") &&
    /^[A-Za-z0-9._/-]+$/.test(value)
  );
}

function safeSubmodulePath(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= 512 &&
    !path.isAbsolute(value) &&
    !value.includes("\\") &&
    /^[A-Za-z0-9._/ -]+$/.test(value) &&
    value.split("/").every((part) => part.length > 0 && part !== "." && part !== "..")
  );
}

function safeFile(root, relative) {
  if (!safeRelativePath(relative)) throw new VerifyError("artifact_unsafe_path");
  const components = relative.split("/");
  let current = root;
  for (const [index, component] of components.entries()) {
    current = path.join(current, component);
    const metadata = fs.lstatSync(current);
    if (metadata.isSymbolicLink()) throw new VerifyError("artifact_unsafe_path");
    if (index < components.length - 1 && !metadata.isDirectory()) {
      throw new VerifyError("artifact_unsafe_path");
    }
    if (index === components.length - 1 && !metadata.isFile()) {
      throw new VerifyError("artifact_missing");
    }
  }
  if (fs.realpathSync(current) !== current || !isWithin(root, fs.realpathSync(current))) {
    throw new VerifyError("artifact_unsafe_path");
  }
  const descriptor = fs.openSync(current, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
  try {
    if (!fs.fstatSync(descriptor).isFile()) throw new VerifyError("artifact_missing");
    return { descriptor, absolutePath: current };
  } catch (error) {
    fs.closeSync(descriptor);
    throw error;
  }
}

function hashSafeFile(root, relative, missingCode) {
  let file;
  try {
    file = safeFile(root, relative);
  } catch (error) {
    if (error?.code === "ENOENT") throw new VerifyError(missingCode);
    if (error instanceof VerifyError && error.code === "artifact_missing") {
      throw new VerifyError(missingCode);
    }
    throw error;
  }
  try {
    const hash = crypto.createHash("sha256");
    const buffer = Buffer.alloc(64 * 1024);
    let count;
    do {
      count = fs.readSync(file.descriptor, buffer, 0, buffer.length, null);
      if (count > 0) hash.update(buffer.subarray(0, count));
    } while (count > 0);
    return { hash: hash.digest("hex"), absolutePath: file.absolutePath };
  } finally {
    fs.closeSync(file.descriptor);
  }
}

function readManifest(manifestPath, artifactRoot) {
  const absolute = path.resolve(manifestPath);
  const parent = path.dirname(absolute);
  if (parent !== artifactRoot || path.basename(absolute) !== MANIFEST_NAME) {
    throw new VerifyError("manifest_invalid");
  }
  let metadata;
  try {
    metadata = fs.lstatSync(absolute);
  } catch (error) {
    if (error?.code === "ENOENT") throw new VerifyError("manifest_missing");
    throw error;
  }
  if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > MAX_MANIFEST_BYTES) {
    throw new VerifyError("manifest_invalid");
  }
  const descriptor = fs.openSync(absolute, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
  try {
    const actualMetadata = fs.fstatSync(descriptor);
    if (!actualMetadata.isFile() || actualMetadata.size > MAX_MANIFEST_BYTES) {
      throw new VerifyError("manifest_invalid");
    }
    try {
      return JSON.parse(fs.readFileSync(descriptor, "utf8"));
    } catch {
      throw new VerifyError("manifest_invalid");
    }
  } finally {
    fs.closeSync(descriptor);
  }
}

function hasExactKeys(value, keys) {
  return (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.keys(value).length === keys.length &&
    keys.every((key) => Object.hasOwn(value, key))
  );
}

function sameStringSet(left, right) {
  return (
    Array.isArray(left) &&
    left.length === right.length &&
    left.every((item) => typeof item === "string") &&
    new Set(left).size === left.length &&
    left.every((item) => right.includes(item))
  );
}

function validCapabilities(capabilities) {
  const advertised = new Set(capabilities);
  const hasPush = advertised.has("push");
  const hasPullRequest = advertised.has("pull_request");
  const hasSecretScan = advertised.has("secret_scan_supported_formats_v1");
  return (
    REQUIRED_CAPABILITIES.every((capability) => advertised.has(capability)) &&
    capabilities.every((capability) => SUPPORTED_CAPABILITIES.includes(capability)) &&
    hasPush === hasSecretScan &&
    (!hasPullRequest || hasPush)
  );
}

function isRevision(value) {
  return typeof value === "string" && /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(value);
}

function isSafeRepository(value) {
  if (typeof value !== "string" || value.length > 2048) return false;
  let parsed;
  try {
    parsed = new URL(value);
  } catch {
    return false;
  }
  if (!value.startsWith(`${parsed.protocol}//`)) return false;
  const authority = value.slice(value.indexOf("://") + 3).split("/")[0];
  if (
    parsed.password ||
    parsed.search ||
    parsed.hash ||
    !parsed.hostname ||
    (parsed.protocol === "https:" && authority.includes("@")) ||
    (parsed.protocol === "ssh:" && authority.includes("@") && !authority.startsWith("git@")) ||
    (parsed.protocol === "https:" && parsed.username) ||
    (parsed.protocol === "ssh:" && parsed.username && parsed.username !== "git")
  ) {
    return false;
  }
  if (!parsed.pathname.startsWith("/") || parsed.pathname.split("/").some((part) => part === "..")) {
    return false;
  }
  return parsed.protocol === "https:" || parsed.protocol === "ssh:";
}

function validSource(source, withSubmodules = false, allowDevelopmentSources = false) {
  const keys = withSubmodules
    ? ["repository", "ref", "revision", "dirty", "submodules"]
    : ["repository", "ref", "revision", "dirty"];
  if (!hasExactKeys(source, keys)) return false;
  const cleanFlag = (dirty) => dirty === false || (allowDevelopmentSources && dirty === true);
  if (
    !isSafeRepository(source.repository) ||
    !validRef(source.ref, allowDevelopmentSources) ||
    !isRevision(source.revision) ||
    !cleanFlag(source.dirty)
  ) {
    return false;
  }
  if (!withSubmodules) return true;
  return (
    Array.isArray(source.submodules) &&
    source.submodules.every(
      (submodule) =>
        hasExactKeys(submodule, ["path", "revision", "dirty"]) &&
        safeSubmodulePath(submodule.path) &&
        isRevision(submodule.revision) &&
        cleanFlag(submodule.dirty),
    ) &&
    new Set(source.submodules.map((submodule) => submodule.path)).size === source.submodules.length
  );
}

function validRef(value, allowFloatingRefs = false) {
  const finalComponent = typeof value === "string" ? value.split("/").at(-1)?.toLowerCase() : "";
  const floating =
    typeof value === "string" &&
    (["main", "master", "head", "latest"].includes(value.toLowerCase()) ||
      ["main", "master", "head", "latest"].includes(finalComponent));
  return (
    typeof value === "string" &&
    value.length <= 128 &&
    /^[A-Za-z0-9][A-Za-z0-9._/-]*$/.test(value) &&
    (allowFloatingRefs || !floating) &&
    !value.includes("..") &&
    !value.includes("//") &&
    !value.endsWith("/") &&
    !value.split("/").some((part) => part.startsWith(".") || part.endsWith(".") || part.endsWith(".lock"))
  );
}

function validManifestShape(manifest, allowDevelopmentSources = false) {
  return (
    hasExactKeys(manifest, [
      "schema_version",
      "profile",
      "target",
      "sources",
      "executor",
      "runtime_compatibility",
      "artifacts",
    ]) &&
    manifest.schema_version === 1 &&
    hasExactKeys(manifest.profile, ["name", "version"]) &&
    manifest.profile.name === "linux-kvm" &&
    Number.isInteger(manifest.profile.version) &&
    hasExactKeys(manifest.target, ["triple"]) &&
    typeof manifest.target.triple === "string" &&
    hasExactKeys(manifest.sources, ["agent_app", "jobworkerp_runtime", "jobworkerp_client"]) &&
    validSource(manifest.sources.agent_app, true, allowDevelopmentSources) &&
    validSource(manifest.sources.jobworkerp_runtime, true, allowDevelopmentSources) &&
    hasExactKeys(manifest.sources.jobworkerp_client, [
      "repository",
      "revision",
      "lockfile_sha256",
    ]) &&
    isSafeRepository(manifest.sources.jobworkerp_client.repository) &&
    isRevision(manifest.sources.jobworkerp_client.revision) &&
    typeof manifest.sources.jobworkerp_client.lockfile_sha256 === "string" &&
    /^[a-f0-9]{64}$/.test(manifest.sources.jobworkerp_client.lockfile_sha256) &&
    hasExactKeys(manifest.executor, ["version", "protocol_version", "capabilities"]) &&
    typeof manifest.executor.version === "string" &&
    /^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?$/.test(manifest.executor.version) &&
    Number.isInteger(manifest.executor.protocol_version) &&
    manifest.executor.protocol_version > 0 &&
    Array.isArray(manifest.executor.capabilities) &&
    manifest.executor.capabilities.every((item) => typeof item === "string") &&
    new Set(manifest.executor.capabilities).size === manifest.executor.capabilities.length &&
    hasExactKeys(manifest.runtime_compatibility, [
      "protocol_version",
      "config_version",
    ]) &&
    hasExactKeys(manifest.artifacts, ["runtime", "executor", "configuration"]) &&
    validArtifactRecord(manifest.artifacts.runtime) &&
    manifest.artifacts.runtime.path === "runtime/all-in-one" &&
    validArtifactRecord(manifest.artifacts.executor) &&
    manifest.artifacts.executor.path === "bin/coding-agent-executor" &&
    Array.isArray(manifest.artifacts.configuration) &&
    manifest.artifacts.configuration.length <= 256 &&
    manifest.artifacts.configuration.every(validArtifactRecord)
  );
}

function validArtifactRecord(artifact) {
  return (
    hasExactKeys(artifact, ["path", "sha256"]) &&
    safeRelativePath(artifact.path) &&
    typeof artifact.sha256 === "string" &&
    /^[a-f0-9]{64}$/.test(artifact.sha256)
  );
}

function verifyConfigurationList(manifest, configRoot) {
  const actualPaths = manifest.artifacts.configuration.map((entry) => entry.path);
  if (
    new Set(actualPaths).size !== actualPaths.length
  ) {
    throw new VerifyError("manifest_invalid");
  }
  for (const entry of manifest.artifacts.configuration) {
    const actual = hashSafeFile(configRoot, entry.path, "configuration_missing");
    if (actual.hash !== entry.sha256) throw new VerifyError("configuration_hash_mismatch");
  }
}

function readSafeFile(root, relative, maxBytes) {
  const file = safeFile(root, relative);
  try {
    const metadata = fs.fstatSync(file.descriptor);
    if (metadata.size > maxBytes) throw new VerifyError("client_lock_invalid");
    return fs.readFileSync(file.descriptor);
  } finally {
    fs.closeSync(file.descriptor);
  }
}

function lockPackageBlocks(lockText) {
  if (lockText.includes("\r") && /\r(?!\n)/.test(lockText)) throw new VerifyError("client_lock_invalid");
  const blocks = [];
  let current;
  for (const line of lockText.replaceAll("\r\n", "\n").split("\n")) {
    // Cargo-generated lockfiles use bare keys; reject quoted/escaped aliases conservatively.
    if (/^[\t ]*(?:"(?:[^"\\]|\\.)*"|'[^']*')[\t ]*=/.test(line)) {
      throw new VerifyError("client_lock_invalid");
    }
    if (/^[\t ]*\[\[package\]\][\t ]*(?:#.*)?$/.test(line)) {
      current = [];
      blocks.push(current);
    } else if (/^[\t ]*\[\[/.test(line)) {
      // Unsupported array-table aliases must not hide additional package records.
      throw new VerifyError("client_lock_invalid");
    } else if (/^[\t ]*\[[^\]]+\][\t ]*(?:#.*)?$/.test(line)) {
      current = undefined;
    } else if (current) {
      current.push(line);
    }
  }
  return blocks;
}

function lockKeyAssignments(block, key) {
  const assignment = new RegExp(`^[\\t ]*${key}[\\t ]*=`);
  return block.filter((line) => assignment.test(line));
}

function lockStringValue(line, key) {
  const match = line.match(/^[\t ]*(name|source)[\t ]*=[\t ]*"([^"\\]*)"[\t ]*(?:#.*)?$/);
  return match?.[1] === key ? match[2] : undefined;
}

function verifyClientLock(manifest, configRoot) {
  const client = manifest.sources.jobworkerp_client;
  const lockContents = readSafeFile(configRoot, "Cargo.lock", 8 * 1024 * 1024);
  const lockHash = crypto.createHash("sha256").update(lockContents).digest("hex");
  if (lockHash !== client.lockfile_sha256) throw new VerifyError("client_lock_hash_mismatch");
  const text = lockContents.toString("utf8");
  if (!Buffer.from(text, "utf8").equals(lockContents)) throw new VerifyError("client_lock_invalid");
  const packages = lockPackageBlocks(text).filter((block) => {
    const names = lockKeyAssignments(block, "name").map((line) => lockStringValue(line, "name"));
    if (!names.includes("jobworkerp-client")) return false;
    if (names.length !== 1 || names[0] !== "jobworkerp-client") {
      throw new VerifyError("client_lock_invalid");
    }
    return true;
  });
  if (packages.length !== 1) throw new VerifyError("client_lock_invalid");
  const sourceFields = lockKeyAssignments(packages[0], "source");
  if (sourceFields.length !== 1) throw new VerifyError("client_lock_invalid");
  const source = lockStringValue(sourceFields[0], "source");
  const sourceMatch = source?.match(/^git\+((?:https|ssh):\/\/[^#]+)#([a-f0-9]{40}|[a-f0-9]{64})$/);
  if (!sourceMatch) throw new VerifyError("client_lock_invalid");
  let repository;
  try {
    const parsed = new URL(sourceMatch[1]);
    if (
      parsed.password ||
      (parsed.protocol === "https:" && parsed.username) ||
      (parsed.protocol === "ssh:" && parsed.username && parsed.username !== "git")
    ) {
      throw new VerifyError("client_lock_invalid");
    }
    parsed.search = "";
    parsed.hash = "";
    repository = `${parsed.protocol}//${parsed.host}${parsed.pathname}`;
    if (!isSafeRepository(repository)) throw new VerifyError("client_lock_invalid");
  } catch (error) {
    if (error instanceof VerifyError) throw error;
    throw new VerifyError("client_lock_invalid");
  }
  if (repository !== client.repository || sourceMatch[2] !== client.revision) {
    throw new VerifyError("client_lock_mismatch");
  }
}

function killProbeProcessGroup(child) {
  if (!child.pid) return;
  if (process.platform !== "win32") {
    try {
      process.kill(-child.pid, "SIGKILL");
      return;
    } catch {
      // Fall through to killing the direct helper if group signaling is unavailable.
    }
  }
  try {
    child.kill("SIGKILL");
  } catch {
    // The process may have exited between the timeout and signal delivery.
  }
}

function runExecutorProbe(binaryPath, args) {
  if (process.platform === "win32") {
    return Promise.reject(new VerifyError("executor_probe_failed"));
  }
  return new Promise((resolve, reject) => {
    let child;
    let timeout;
    let settled = false;
    let outputBytes = 0;
    const output = [];
    const finishFailure = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      unregisterProbeCleanup(finishFailure);
      if (child) killProbeProcessGroup(child);
      reject(new VerifyError("executor_probe_failed"));
    };

    try {
      child = spawn(binaryPath, args, {
        cwd: path.dirname(binaryPath),
        detached: true,
        env: { PATH: "/usr/bin:/bin", LANG: "C", LC_ALL: "C" },
        stdio: ["ignore", "pipe", "ignore"],
        windowsHide: true,
      });
    } catch {
      finishFailure();
      return;
    }
    registerProbeCleanup(finishFailure);

    child.stdout.on("data", (chunk) => {
      if (settled) return;
      outputBytes += chunk.length;
      if (outputBytes > MAX_PROBE_OUTPUT_BYTES) {
        finishFailure();
        return;
      }
      output.push(chunk);
    });
    child.on("error", finishFailure);
    child.on("close", (status, signal) => {
      if (settled) return;
      if (status !== 0 || signal !== null) {
        finishFailure();
        return;
      }
      settled = true;
      clearTimeout(timeout);
      unregisterProbeCleanup(finishFailure);
      // A helper must not leave a background process behind even after a successful query.
      killProbeProcessGroup(child);
      resolve(Buffer.concat(output).toString("utf8").trim());
    });

    timeout = setTimeout(finishFailure, PROBE_TIMEOUT_MS);
  });
}

async function verifyExecutor(manifest, binaryPath, expectedHash) {
  if (!validCapabilities(manifest.executor.capabilities)) {
    throw new VerifyError("executor_capabilities_unsupported");
  }
  const scratchMetadata = fs.lstatSync("/tmp/opencode");
  if (!scratchMetadata.isDirectory() || scratchMetadata.isSymbolicLink()) {
    throw new VerifyError("artifact_unsafe_path");
  }
  const probeRoot = fs.mkdtempSync("/tmp/opencode/coding-agent-probe-");
  const probeBinary = path.join(probeRoot, "coding-agent-executor");
  try {
    fs.copyFileSync(binaryPath, probeBinary, fs.constants.COPYFILE_EXCL);
    fs.chmodSync(probeBinary, 0o700);
    if (hashFile(probeBinary) !== expectedHash) throw new VerifyError("artifact_hash_mismatch");
    await verifyExecutorCli(manifest, probeBinary);
  } finally {
    fs.rmSync(probeRoot, { recursive: true, force: true });
  }
}

function hashFile(filePath) {
  const descriptor = fs.openSync(filePath, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
  try {
    const hash = crypto.createHash("sha256");
    const buffer = Buffer.alloc(64 * 1024);
    let count;
    do {
      count = fs.readSync(descriptor, buffer, 0, buffer.length, null);
      if (count > 0) hash.update(buffer.subarray(0, count));
    } while (count > 0);
    return hash.digest("hex");
  } finally {
    fs.closeSync(descriptor);
  }
}

async function verifyExecutorCli(manifest, binaryPath) {
  const versionOutput = await runExecutorProbe(binaryPath, ["--version"]);
  const expectedVersion = `coding-agent-executor ${manifest.executor.version} protocol=${manifest.executor.protocol_version}`;
  if (versionOutput !== expectedVersion) throw new VerifyError("executor_version_mismatch");

  let capabilities;
  try {
    capabilities = JSON.parse(await runExecutorProbe(binaryPath, ["--capabilities"]));
  } catch (error) {
    if (error instanceof VerifyError) throw error;
    throw new VerifyError("executor_capabilities_invalid");
  }
  if (!hasExactKeys(capabilities, ["protocol_version", "executor_version", "features"])) {
    throw new VerifyError("executor_capabilities_invalid");
  }
  if (
    capabilities.protocol_version !== manifest.executor.protocol_version ||
    capabilities.executor_version !== manifest.executor.version
  ) {
    throw new VerifyError("executor_protocol_mismatch");
  }
  if (
    !Array.isArray(capabilities.features) ||
    !sameStringSet(capabilities.features, manifest.executor.capabilities)
  ) {
    throw new VerifyError("executor_capabilities_mismatch");
  }
}

async function verify(options) {
  const artifactRoot = canonicalDirectory(options.artifactroot);
  const configRoot = canonicalDirectory(options.configroot);
  const manifest = readManifest(options.manifest, artifactRoot);
  if (!validManifestShape(manifest, options.allowDevelopmentSources)) {
    throw new VerifyError("manifest_invalid");
  }
  if (manifest.profile.version !== 1) throw new VerifyError("unsupported_profile_version");
  if (!SUPPORTED_TARGETS.has(manifest.target.triple)) throw new VerifyError("unsupported_target");
  if (manifest.executor.protocol_version !== SUPPORTED_PROTOCOL_VERSION) {
    throw new VerifyError("executor_protocol_unsupported");
  }
  if (
    manifest.runtime_compatibility.protocol_version !== null ||
    manifest.runtime_compatibility.config_version !== null
  ) {
    throw new VerifyError("runtime_compatibility_unverified");
  }

  const runtime = hashSafeFile(artifactRoot, manifest.artifacts.runtime.path, "artifact_missing");
  const executor = hashSafeFile(artifactRoot, manifest.artifacts.executor.path, "artifact_missing");
  if (runtime.hash !== manifest.artifacts.runtime.sha256 || executor.hash !== manifest.artifacts.executor.sha256) {
    throw new VerifyError("artifact_hash_mismatch");
  }
  verifyConfigurationList(manifest, configRoot);
  verifyClientLock(manifest, configRoot);
  await verifyExecutor(manifest, executor.absolutePath, manifest.artifacts.executor.sha256);

  return {
    provenance_valid: true,
    enable_ready: false,
    target: manifest.target.triple,
    executor: manifest.executor,
    diagnostics: [
      "installed_runtime_protocol_compatibility_unverified",
      "installed_runtime_config_compatibility_unverified",
      ...(options.allowDevelopmentSources ? ["development_sources_accepted"] : []),
    ],
  };
}

function writeReport(report, json) {
  if (json) {
    process.stdout.write(`${JSON.stringify(report)}\n`);
    return;
  }
  process.stdout.write(
    `${report.provenance_valid ? "PROVENANCE VALID" : "PROVENANCE INVALID"}; ` +
      `${report.enable_ready ? "coding-agent execution enabled" : "coding-agent execution remains disabled"}\n` +
      `${report.diagnostics.join("\n")}\n`,
  );
}

async function main() {
  let options = { json: false, provenanceOnly: false, allowDevelopmentSources: false };
  let report;
  try {
    options = parseArguments(process.argv.slice(2));
    report = await verify(options);
  } catch (error) {
    const code = error instanceof VerifyError ? error.code : "verification_failed";
    report = { provenance_valid: false, enable_ready: false, diagnostics: [code] };
  }

  writeReport(report, options.json);
  if (!report.provenance_valid || !options.provenanceOnly) {
    process.exitCode = terminationSignal === "SIGINT" ? 130 : terminationSignal === "SIGTERM" ? 143 : 2;
  }
}

await main();
