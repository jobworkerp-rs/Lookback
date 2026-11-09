#!/usr/bin/env node
// Record coding-agent artifact provenance as `coding-agent-artifact-manifest.json`.
//
// Two callers share this module so the manifest shape has a single definition:
// - scripts/build-coding-agent-executor.sh (release): every source pin is passed
//   explicitly, sources must be clean, and the manifest is created exclusively.
// - scripts/stage-dev-external-bins.sh (development): pins are derived from the
//   local checkouts, uncommitted changes and branch heads are recorded
//   truthfully (`dirty: true`, `ref: <branch>`), and an existing manifest is
//   replaced atomically. Only debug builds of the app accept such a manifest.
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";
import { credentialFreeOrigin } from "./git-origin.mjs";

export const MANIFEST_NAME = "coding-agent-artifact-manifest.json";
export const EXECUTOR_CAPABILITIES = [
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

const PIN_OPTIONS = [
  "app-url",
  "app-ref",
  "app-revision",
  "runtime-url",
  "runtime-ref",
  "runtime-revision",
  "client-revision",
];

function git(root, ...args) {
  return execFileSync(
    "git",
    ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C", root, ...args],
    {
      encoding: "buffer",
      env: {
        ...process.env,
        GIT_CONFIG_NOSYSTEM: "1",
        GIT_CONFIG_GLOBAL: "/dev/null",
        GIT_TERMINAL_PROMPT: "0",
      },
    },
  );
}

function gitText(root, ...args) {
  const output = git(root, ...args).toString("utf8");
  return output.endsWith("\n") ? output.slice(0, -1) : output;
}

function safeRelative(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    !path.isAbsolute(value) &&
    !value.includes("\\") &&
    value.split("/").every((part) => part && part !== "." && part !== "..") &&
    /^[A-Za-z0-9._/-]+$/.test(value)
  );
}

function sha256File(root, relative) {
  if (!safeRelative(relative)) throw new Error();
  let current = root;
  for (const component of relative.split("/")) {
    current = path.join(current, component);
    const metadata = fs.lstatSync(current);
    if (metadata.isSymbolicLink()) throw new Error();
  }
  const rootCanonical = fs.realpathSync(root);
  if (fs.realpathSync(current) !== current || !current.startsWith(`${rootCanonical}${path.sep}`)) {
    throw new Error();
  }
  const descriptor = fs.openSync(current, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
  try {
    if (!fs.fstatSync(descriptor).isFile()) throw new Error();
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

function lockPackageBlocks(lockText) {
  if (lockText.includes("\r") && /\r(?!\n)/.test(lockText)) throw new Error();
  const blocks = [];
  let current;
  for (const line of lockText.replaceAll("\r\n", "\n").split("\n")) {
    // Cargo-generated lockfiles use bare keys; reject quoted/escaped aliases conservatively.
    if (/^[\t ]*(?:"(?:[^"\\]|\\.)*"|'[^']*')[\t ]*=/.test(line)) throw new Error();
    if (/^[\t ]*\[\[package\]\][\t ]*(?:#.*)?$/.test(line)) {
      current = [];
      blocks.push(current);
    } else if (/^[\t ]*\[\[/.test(line)) {
      // Unsupported array-table aliases must not hide additional package records.
      throw new Error();
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

/**
 * Locate the single `jobworkerp-client` git source in a Cargo.lock. When
 * `expectedRevision` is given the locked revision must match it.
 */
export function lockedClientSource(lockText, expectedRevision) {
  const packages = lockPackageBlocks(lockText).filter((block) => {
    const names = lockKeyAssignments(block, "name").map((line) => lockStringValue(line, "name"));
    if (!names.includes("jobworkerp-client")) return false;
    if (names.length !== 1 || names[0] !== "jobworkerp-client") throw new Error();
    return true;
  });
  if (packages.length !== 1) throw new Error();
  const sourceFields = lockKeyAssignments(packages[0], "source");
  if (sourceFields.length !== 1) throw new Error();
  const source = lockStringValue(sourceFields[0], "source");
  const sourceMatch = source?.match(/^git\+((?:https|ssh):\/\/[^#]+)#([a-f0-9]{40}|[a-f0-9]{64})$/);
  if (!sourceMatch) throw new Error();
  if (expectedRevision !== undefined && sourceMatch[2] !== expectedRevision) throw new Error();
  const parsed = new URL(sourceMatch[1]);
  if (
    parsed.password ||
    (parsed.protocol === "https:" && parsed.username) ||
    (parsed.protocol === "ssh:" && parsed.username && parsed.username !== "git")
  ) {
    throw new Error();
  }
  parsed.search = "";
  parsed.hash = "";
  return { repository: `${parsed.protocol}//${parsed.host}${parsed.pathname}`, revision: sourceMatch[2] };
}

function safeSubmodulePath(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= 512 &&
    !path.isAbsolute(value) &&
    !value.includes("\\") &&
    value.split("/").every((part) => part && part !== "." && part !== "..") &&
    /^[A-Za-z0-9._/ -]+$/.test(value)
  );
}

function worktreeDirty(root) {
  return (
    git(root, "status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none").length !== 0
  );
}

function gitlinks(root) {
  return git(root, "ls-tree", "-r", "-z", "HEAD")
    .toString("utf8")
    .split("\0")
    .filter(Boolean)
    .flatMap((record) => {
      const separator = record.indexOf("\t");
      if (separator < 0) throw new Error();
      const [mode, objectType, revision] = record.slice(0, separator).split(" ");
      if (mode !== "160000") return [];
      const submodulePath = record.slice(separator + 1);
      if (
        objectType !== "commit" ||
        !/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(revision) ||
        !safeSubmodulePath(submodulePath)
      ) {
        throw new Error();
      }
      return [{ path: submodulePath, revision }];
    });
}

/**
 * Walk the initialized submodule tree. In release mode any dirty worktree or a
 * checkout that differs from the recorded gitlink aborts; in development mode
 * the dirty flag is recorded per repository and the actual checked-out
 * revision is used.
 */
function submodules(root, allowDevelopmentSources) {
  const result = [];
  function visit(repository, prefix) {
    if (!allowDevelopmentSources && worktreeDirty(repository)) throw new Error();
    for (const entry of gitlinks(repository)) {
      let childPath = repository;
      for (const component of entry.path.split("/")) {
        childPath = path.join(childPath, component);
        const metadata = fs.lstatSync(childPath);
        if (metadata.isSymbolicLink()) throw new Error();
      }
      if (!fs.statSync(childPath).isDirectory()) throw new Error();
      const canonicalChild = fs.realpathSync(childPath);
      const topLevel = fs.realpathSync(gitText(childPath, "rev-parse", "--show-toplevel"));
      if (canonicalChild !== topLevel) throw new Error();
      const checkedOut = gitText(childPath, "rev-parse", "HEAD");
      if (!allowDevelopmentSources && checkedOut !== entry.revision) throw new Error();
      const relative = prefix ? `${prefix}/${entry.path}` : entry.path;
      if (!safeSubmodulePath(relative)) throw new Error();
      const dirty = allowDevelopmentSources
        ? worktreeDirty(childPath) || checkedOut !== entry.revision
        : false;
      result.push({ path: relative, revision: checkedOut, dirty });
      visit(childPath, relative);
    }
  }
  visit(root, "");
  return result;
}

/**
 * Describe one source checkout. Explicit `pins` win; in development mode the
 * missing values are read from the checkout itself.
 */
export function describeSource(root, pins, allowDevelopmentSources) {
  const repository =
    pins.repository ??
    (allowDevelopmentSources
      ? credentialFreeOrigin(gitText(root, "remote", "get-url", "origin"), "source origin")
      : undefined);
  const ref =
    pins.ref ??
    (allowDevelopmentSources ? currentBranch(root) : undefined);
  const revision = pins.revision ?? (allowDevelopmentSources ? gitText(root, "rev-parse", "HEAD") : undefined);
  if (!repository || !ref || !revision) throw new Error();
  const dirty = allowDevelopmentSources ? worktreeDirty(root) : false;
  return { repository, ref, revision, dirty, submodules: submodules(root, allowDevelopmentSources) };
}

function currentBranch(root) {
  try {
    return gitText(root, "symbolic-ref", "--short", "-q", "HEAD");
  } catch (error) {
    // `symbolic-ref -q` exits 1 on a detached HEAD, which has no branch name;
    // the manifest records the literal `HEAD` so the development source policy
    // still identifies a floating ref.
    if (error?.status === 1) return "HEAD";
    throw error;
  }
}

function executorIdentity(appRoot) {
  const packageText = fs.readFileSync(path.join(appRoot, "coding-agent-executor/Cargo.toml"), "utf8");
  const version = packageText.match(/^version = "([0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?)"$/m)?.[1];
  const modelText = fs.readFileSync(path.join(appRoot, "coding-agent-executor/src/model.rs"), "utf8");
  const protocolVersion = Number(modelText.match(/^pub const PROTOCOL_VERSION: u32 = ([0-9]+);$/m)?.[1]);
  if (!version || !Number.isSafeInteger(protocolVersion) || protocolVersion < 1) throw new Error();
  return { version, protocol_version: protocolVersion, capabilities: [...EXECUTOR_CAPABILITIES] };
}

function clientSource(appRoot, expectedRevision) {
  const lockPath = path.join(appRoot, "Cargo.lock");
  const lockMetadata = fs.lstatSync(lockPath);
  if (!lockMetadata.isFile() || lockMetadata.isSymbolicLink() || lockMetadata.size > 8 * 1024 * 1024) {
    throw new Error();
  }
  const lockContents = fs.readFileSync(lockPath);
  const lockText = lockContents.toString("utf8");
  if (!Buffer.from(lockText, "utf8").equals(lockContents)) throw new Error();
  const client = lockedClientSource(lockText, expectedRevision);
  client.lockfile_sha256 = crypto.createHash("sha256").update(lockContents).digest("hex");
  return client;
}

/**
 * Build the manifest object for an artifact root that already contains
 * `runtime/all-in-one` and `bin/coding-agent-executor`.
 *
 * @param {object} options
 * @param {string} options.appRoot agent-app checkout
 * @param {string} options.runtimeRoot jobworkerp runtime checkout
 * @param {string} options.artifactRoot staged artifact directory
 * @param {string} options.target Rust target triple
 * @param {object} options.pins explicit `app`, `runtime` ({repository, ref, revision}) and `clientRevision`
 * @param {boolean} options.allowDevelopmentSources record working-tree provenance instead of failing
 */
export function buildManifest({ appRoot, runtimeRoot, artifactRoot, target, pins = {}, allowDevelopmentSources = false }) {
  if (!allowDevelopmentSources) {
    const required = [pins.app?.repository, pins.app?.ref, pins.app?.revision, pins.runtime?.repository, pins.runtime?.ref, pins.runtime?.revision, pins.clientRevision];
    if (!required.every(Boolean)) throw new Error();
  }
  return {
    schema_version: 1,
    profile: { name: "linux-kvm", version: 1 },
    target: { triple: target },
    sources: {
      agent_app: describeSource(appRoot, pins.app ?? {}, allowDevelopmentSources),
      jobworkerp_runtime: describeSource(runtimeRoot, pins.runtime ?? {}, allowDevelopmentSources),
      jobworkerp_client: clientSource(appRoot, pins.clientRevision),
    },
    executor: executorIdentity(appRoot),
    runtime_compatibility: {
      protocol_version: null,
      config_version: null,
    },
    artifacts: {
      runtime: { path: "runtime/all-in-one", sha256: sha256File(artifactRoot, "runtime/all-in-one") },
      executor: {
        path: "bin/coding-agent-executor",
        sha256: sha256File(artifactRoot, "bin/coding-agent-executor"),
      },
      configuration: [],
    },
  };
}

/**
 * Write the manifest. Release staging creates the file exclusively; development
 * staging replaces an existing manifest atomically so a re-run is idempotent.
 */
export function writeManifest(outputPath, manifest, { replace = false } = {}) {
  const bytes = `${JSON.stringify(manifest, null, 2)}\n`;
  if (!replace) {
    fs.writeFileSync(outputPath, bytes, { flag: "wx", mode: 0o600 });
    return;
  }
  const temporary = `${outputPath}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, bytes, { flag: "wx", mode: 0o644 });
  try {
    fs.renameSync(temporary, outputPath);
  } catch (error) {
    fs.rmSync(temporary, { force: true });
    throw error;
  }
}

function parseArguments(argv) {
  const options = { allowDevelopmentSources: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--allow-development-sources") {
      options.allowDevelopmentSources = true;
      continue;
    }
    const name = argument.startsWith("--") ? argument.slice(2) : undefined;
    const valued = ["app-root", "runtime-root", "artifact-root", "target", "output", ...PIN_OPTIONS];
    if (!name || !valued.includes(name)) throw new Error();
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) throw new Error();
    options[name] = value;
    index += 1;
  }
  for (const required of ["app-root", "runtime-root", "artifact-root", "target"]) {
    if (!options[required]) throw new Error();
  }
  return options;
}

function main(argv) {
  const options = parseArguments(argv);
  const manifest = buildManifest({
    appRoot: options["app-root"],
    runtimeRoot: options["runtime-root"],
    artifactRoot: options["artifact-root"],
    target: options.target,
    pins: {
      app: { repository: options["app-url"], ref: options["app-ref"], revision: options["app-revision"] },
      runtime: {
        repository: options["runtime-url"],
        ref: options["runtime-ref"],
        revision: options["runtime-revision"],
      },
      clientRevision: options["client-revision"],
    },
    allowDevelopmentSources: options.allowDevelopmentSources,
  });
  writeManifest(options.output ?? path.join(options["artifact-root"], MANIFEST_NAME), manifest, {
    replace: options.allowDevelopmentSources,
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    main(process.argv.slice(2));
  } catch {
    // The fixed message keeps repository paths, URLs, and lock contents out of
    // build logs; callers inspect the exit status.
    process.stderr.write("error: could not record coding-agent artifact provenance\n");
    process.exitCode = 1;
  }
}
