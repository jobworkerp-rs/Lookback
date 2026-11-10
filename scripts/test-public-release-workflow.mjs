import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..");
const workflowPath = process.argv[2]
  ? path.resolve(process.argv[2])
  : path.join(repoRoot, ".github", "workflows", "release.yml");

function read(file) {
  try {
    return fs.readFileSync(file, "utf8").replace(/\r\n/g, "\n");
  } catch (error) {
    throw new Error(`cannot read ${file}: ${error.message}`, { cause: error });
  }
}

function jobBlock(workflow, name) {
  const start = workflow.indexOf(`  ${name}:`);
  assert.notEqual(start, -1, `release workflow is missing job ${name}`);
  const next = workflow.slice(start + 1).search(/^  [A-Za-z0-9_-]+:/m);
  return workflow.slice(start, next === -1 ? undefined : start + 1 + next);
}

function stepBlock(job, marker) {
  const at = job.indexOf(marker);
  assert.notEqual(at, -1, `job is missing a step containing ${marker}`);
  const start = job.lastIndexOf("\n      - ", at);
  const next = job.indexOf("\n      - ", at);
  return job.slice(start, next === -1 ? undefined : next);
}

const tagPushOnly = /^ +if: (?:.+ && )?github\.event_name == 'push'$/m;

const workflow = read(workflowPath);

// The private gitea source tracks this file only to publish it. Gitea ignores
// .github/workflows while .gitea/workflows exists, but every job must still
// refuse to run anywhere other than GitHub should that ever change.
const jobsSection = workflow.slice(workflow.search(/^jobs:$/m));
const jobNames = [...jobsSection.matchAll(/^  ([A-Za-z0-9_-]+):$/gm)].map((match) => match[1]);
assert.ok(jobNames.length > 0, "release workflow defines no jobs");
for (const name of jobNames) {
  assert.match(
    jobBlock(workflow, name),
    /^    if: github\.server_url == 'https:\/\/github\.com'(?: && .+)?$/m,
    `job ${name} must run only on GitHub`,
  );
}

// A manual run rebuilds the Linux bundles to verify a runner image change
// without publishing anything.
assert.match(workflow, /^  workflow_dispatch:/m,
  "the release workflow must be runnable without pushing a release tag");
assert.match(workflow,
  /^  LOOKBACK_RELEASE_VERSION: \$\{\{ github\.event_name == 'push' && github\.ref_name \|\| '' \}\}$/m,
  "a manual run has a branch name, not a version, so it must not stamp one");
assert.match(workflow, /^  cancel-in-progress: \$\{\{ github\.event_name == 'workflow_dispatch' \}\}$/m,
  "only a manual run may be cancelled; a tag run must not stop mid-upload");

// The self-hosted Linux runner gives jobs no root access and mounts no FUSE.
// It is shared, so its jobs get a read-only token and no secrets even on a tag
// push; only the jobs that publish raise the token to write.
assert.match(workflow, /^permissions:\n  contents: read$/m,
  "jobs must default to a read-only token");
assert.deepEqual(
  jobNames.filter((name) => /^    permissions:\n      contents: write$/m.test(jobBlock(workflow, name))).sort(),
  ["publish-linux", "publish-macos"],
  "only the publishing jobs may write to the release",
);
const selfHostedLinuxJobs = jobNames.filter((name) =>
  /^    runs-on: lookback-linux-cuda$/m.test(jobBlock(workflow, name)));
assert.deepEqual(selfHostedLinuxJobs.sort(), ["build", "build-cuda", "test"]);
for (const name of selfHostedLinuxJobs) {
  const job = jobBlock(workflow, name);
  assert.doesNotMatch(job, /\bsudo\b/, `job ${name} must not need root on the self-hosted runner`);
  assert.doesNotMatch(job, /\bapt(?:-get)?\b/, `job ${name} must take system packages from the runner image`);
  assert.doesNotMatch(job, /^    permissions:/m, `job ${name} must keep the read-only token on the shared runner`);
  assert.doesNotMatch(job, /\bsecrets\./, `job ${name} must not expose secrets to the shared runner`);
}
for (const name of ["build-cuda", "build"]) {
  assert.match(jobBlock(workflow, name), /^      APPIMAGE_EXTRACT_AND_RUN: "1"$/m,
    `job ${name} must extract AppImage tools instead of mounting them through FUSE`);
}

// Later steps run dependency code, so no checkout may leave the token in
// .git/config.
for (const name of jobNames.filter((job) => jobBlock(workflow, job).includes("uses: actions/checkout@"))) {
  assert.match(stepBlock(jobBlock(workflow, name), "uses: actions/checkout@"), /^          persist-credentials: false$/m,
    `job ${name} must not leave the token in the checkout`);
}

// A tag can be moved by the action's owner; only a commit SHA fixes the code
// that runs next to the Apple secrets. The comment keeps the version readable.
const actionRefs = [...workflow.matchAll(/^\s+(?:- )?uses: (.+)$/gm)].map((match) => match[1]);
assert.ok(actionRefs.length > 0, "release workflow uses no actions");
for (const ref of actionRefs) {
  assert.match(ref, /^[\w.-]+\/[\w.\/-]+@[0-9a-f]{40} # \S+$/,
    `action must be pinned to a commit SHA with a version comment: ${ref}`);
}

const test = jobBlock(workflow, "test");
const runnerTools = stepBlock(test, "Verify runner image tools").split(/[\s;]+/);
for (const tool of "protoc jq cmake pkg-config nvcc objdump patchelf appimagetool desktop-file-validate xdg-open".split(" ")) {
  assert.ok(runnerTools.includes(tool), `the test job must check the runner image provides ${tool}`);
}
assert.match(test, /mkdir -p src-tauri\/migration-bundle/,
  "the test job must create the resource directory removed from source control");
assert.match(test, /bash scripts\/ci-stage-placeholders\.sh/,
  "the test job must stage external-bin and plugin placeholders for tauri-build");

// A manual run signs and notarizes the macOS DMG like a release.
const mac = jobBlock(workflow, "build-macos");
assert.deepEqual(mac.match(/^ +if: .*github\.event_name.*$/gm),
  [stepBlock(mac, "uses: actions/upload-artifact@").match(tagPushOnly)[0]],
  "a manual run must run every macOS step except keeping the DMG");
assert.doesNotMatch(mac, /^    permissions:/m, "the macOS build must keep the read-only token");
assert.doesNotMatch(mac, /softprops\/action-gh-release/, "the macOS build must not publish the DMG itself");
assert.match(mac, /brew install protobuf cmake pkgconf jq/);
assert.match(mac, /command -v protoc/,
  "the macOS build must check the host protobuf compiler before the release build");
assert.match(mac, /command -v jq/);
for (const secret of [
  "APPLE_CERTIFICATE",
  "APPLE_CERTIFICATE_PASSWORD",
  "APPLE_SIGNING_IDENTITY",
  "APPLE_ID",
  "APPLE_PASSWORD",
  "APPLE_TEAM_ID",
]) {
  assert.ok(mac.includes(`secrets.${secret}`), `macOS signing must retain the ${secret} secret mapping`);
}
const macBuild = stepBlock(mac, "bash scripts/build-release.sh --profile mac");
assert.match(macBuild, /APPLE_SIGNING_IDENTITY: \$\{\{ secrets\.APPLE_SIGNING_IDENTITY \}\}/);
assert.doesNotMatch(macBuild, /APPLE_(?:CERTIFICATE|ID|PASSWORD|TEAM_ID)\b/,
  "the build step must not receive Apple secrets it does not use");
assert.match(mac, /^      SIGNING_KEYCHAIN: signing_temp_\$\{\{ github\.run_id \}\}_\$\{\{ github\.run_attempt \}\}$/m,
  "each run must sign from its own keychain");
assert.ok(stepBlock(mac, "uses: apple-actions/import-codesign-certs@").includes("keychain: ${{ env.SIGNING_KEYCHAIN }}"),
  "the certificate must be imported into this run's keychain");
const keychainCleanup = "name: Delete Apple signing keychain";
const cleanup = stepBlock(mac, keychainCleanup);
assert.match(cleanup, /^        if: always\(\)$/m, "the keychain must be deleted even when the job fails");
assert.ok(cleanup.includes("${SIGNING_KEYCHAIN}.keychain-db"), "the cleanup must delete the keychain this run imported into");
assert.equal(mac.indexOf("\n      - ", mac.indexOf(keychainCleanup)), -1,
  "the keychain cleanup must stay the final step");
assert.match(mac, /node scripts\/verify-migration-release\.mjs[\s\S]*?--distributed-assets/,
  "the assembled app must be checked against signed migration provenance and packaged bytes");
assert.match(mac, /xcrun notarytool submit/);
assert.match(mac, /xcrun stapler staple/);
assert.match(mac, /xcrun stapler validate/);
assert.match(mac, /target\/release\/bundle\/dmg\/\*\.dmg/);

const cuda = jobBlock(workflow, "build-cuda");
assert.match(cuda, /needs: test/);

const cpu = jobBlock(workflow, "build");
assert.match(cpu, /needs: \[test, build-cuda\]/,
  "the CPU build must retain its dependency on the CUDA release job");

function assertPublishes(publishName, builds, files) {
  const publish = jobBlock(workflow, publishName);
  assert.match(publish, /^    runs-on: ubuntu-latest$/m, `${publishName} must run off the self-hosted runners`);
  const needs = publish.match(/^    needs: \[?([^\]\n]+)\]?$/m);
  assert.deepEqual(needs?.[1].split(", "), builds, `${publishName} must wait for ${builds.join(", ")}`);
  assert.match(publish, tagPushOnly, `a manual run must not run ${publishName}`);
  const downloaded = stepBlock(publish, "uses: actions/download-artifact@").match(/^          pattern: (\S+)$/m);
  assert.ok(downloaded, `${publishName} must download the bundles by pattern`);
  const downloadPattern = new RegExp(`^${downloaded[1].replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`);
  for (const name of builds) {
    const upload = stepBlock(jobBlock(workflow, name), "uses: actions/upload-artifact@");
    assert.match(upload, tagPushOnly, `a manual run must not keep the bundles of ${name}`);
    const kept = upload.match(/^          name: (\S+)$/m);
    assert.ok(kept && downloadPattern.test(kept[1]), `${publishName} must download the bundles kept by ${name}`);
  }
  const published = stepBlock(publish, "uses: softprops/action-gh-release@");
  for (const file of files) {
    assert.ok(published.includes(file), `${publishName} must attach ${file}`);
  }
}
assertPublishes("publish-linux", ["build-cuda", "build"], ["*.deb", "*.AppImage"]);
assertPublishes("publish-macos", ["build-macos"], ["*.dmg"]);

// Releases ship the Issue Tracker server only with --experimental-issues, so
// no release job builds it or caches its target directory.
for (const name of jobNames) {
  assert.doesNotMatch(jobBlock(workflow, name), /issue-tracker-server|--experimental-issues/,
    `job ${name} must not build or bundle the experimental Issue Tracker server`);
}

console.log("public release workflow checks passed");
