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

// The macOS app is built on the self-hosted runner without any Apple secret,
// then signed and notarized on a GitHub-hosted runner, so the signing key
// never shares a machine with the dependency code the build runs. A manual
// run signs and notarizes like a release; only keeping the DMG is tag-only.
const macApple = [
  "APPLE_CERTIFICATE",
  "APPLE_CERTIFICATE_PASSWORD",
  "APPLE_SIGNING_IDENTITY",
  "APPLE_NOTARY_KEY",
  "APPLE_NOTARY_KEY_ID",
  "APPLE_NOTARY_ISSUER_ID",
];
// Notarization uses an App Store Connect API key, which can be scoped and
// revoked on its own, instead of an Apple ID app-specific password.
assert.doesNotMatch(workflow, /APPLE_(?:ID|PASSWORD|TEAM_ID)\b|--apple-id|--password /,
  "notarization must not use Apple ID credentials");
const mac = jobBlock(workflow, "build-macos");
assert.match(mac, /^    runs-on: \[self-hosted, macOS, lookback-macos\]$/m);
assert.doesNotMatch(mac, /\bsecrets\./, "the self-hosted macOS build must not receive any secret");
assert.doesNotMatch(mac, /codesign|keychain|notarytool|import-codesign-certs/,
  "the self-hosted macOS build must not sign or notarize");
assert.doesNotMatch(mac, /^ +if: .*github\.event_name.*$/m, "a manual run must build the macOS app too");
assert.doesNotMatch(mac, /^    permissions:/m, "the macOS build must keep the read-only token");
assert.match(mac, /brew install protobuf cmake pkgconf jq/);
assert.match(mac, /command -v protoc/,
  "the macOS build must check the host protobuf compiler before the release build");
assert.match(mac, /command -v jq/);
assert.match(mac, /bash scripts\/build-release\.sh --profile mac --macos-unsigned-intermediate --bundle app$/m,
  "the self-hosted macOS build must stop at the verified unsigned app");
// What travels and how it is checked lives in scripts/macos-signing-handoff.sh
// (covered by scripts/test-macos-signing-handoff.sh).
const handoff = stepBlock(mac, "name: Package the unsigned app");
assert.match(handoff, /bash scripts\/macos-signing-handoff\.sh pack /);
assert.match(mac, /^      sha256: \$\{\{ steps\.package\.outputs\.sha256 \}\}$/m,
  "the build must publish the hand-off checksum as a job output");
const macKept = stepBlock(mac, "uses: actions/upload-artifact@").match(/^          name: (\S+)$/m);
assert.equal(macKept?.[1], "macos-unsigned-app");

const sign = jobBlock(workflow, "sign-macos");
assert.match(sign, /^    runs-on: macos-\d+$/m, "signing must run on a GitHub-hosted macOS runner");
assert.doesNotMatch(sign.match(/^    runs-on: .*$/m)?.[0] ?? "", /self-hosted/,
  "signing must not run on the self-hosted runner");
assert.match(sign, /^    needs: build-macos$/m);
assert.match(sign, /^    environment: release-macos$/m, "the Apple secrets must come from the release-macos environment");
assert.doesNotMatch(sign, /^    permissions:/m, "the signing job must keep the read-only token");
for (const secret of macApple) {
  assert.ok(sign.includes(`secrets.${secret}`), `macOS signing must retain the ${secret} secret mapping`);
}
assert.equal(stepBlock(sign, "uses: actions/download-artifact@").match(/^          name: (\S+)$/m)?.[1],
  "macos-unsigned-app", "signing must download the app the build kept");
const verifyHandoff = stepBlock(sign, "name: Verify and unpack the unsigned app");
assert.match(verifyHandoff, /bash scripts\/macos-signing-handoff\.sh unpack /,
  "the hand-off must be unpacked by the script that checks its checksum and members");
assert.ok(verifyHandoff.includes("${{ needs.build-macos.outputs.sha256 }}"),
  "the hand-off must be checked against the checksum the build published");
assert.doesNotMatch(verifyHandoff, /\btar -x/, "the hand-off must not be unpacked without its checks");
const order = (first, second) => {
  const a = sign.indexOf(first);
  const b = sign.indexOf(second);
  assert.ok(a !== -1 && b !== -1 && a < b, `expected ${first} before ${second} in sign-macos`);
};
order("name: Verify and unpack the unsigned app", "uses: apple-actions/import-codesign-certs@");
order("uses: apple-actions/import-codesign-certs@", "bash scripts/finalize-macos-bundle.sh");
order("bash scripts/finalize-macos-bundle.sh", "xcrun notarytool submit");
order("xcrun notarytool submit", "spctl -a -t open --context context:primary-signature");
const finalize = stepBlock(sign, "bash scripts/finalize-macos-bundle.sh");
assert.match(finalize, /--dmg target\/release\/bundle\/dmg\/Lookback\.dmg/);
assert.match(finalize, /APPLE_SIGNING_IDENTITY: \$\{\{ secrets\.APPLE_SIGNING_IDENTITY \}\}/);
assert.doesNotMatch(finalize, /APPLE_(?:CERTIFICATE|ID|PASSWORD|TEAM_ID)\b/,
  "the finalize step must not receive Apple secrets it does not use");
assert.deepEqual(sign.match(/^ +if: .*github\.event_name.*$/gm),
  [stepBlock(sign, "uses: actions/upload-artifact@").match(tagPushOnly)[0]],
  "a manual run must sign and notarize; only keeping the DMG is tag-only");
assert.match(sign, /^      SIGNING_KEYCHAIN: signing_temp_\$\{\{ github\.run_id \}\}_\$\{\{ github\.run_attempt \}\}$/m,
  "each run must sign from its own keychain");
assert.ok(stepBlock(sign, "uses: apple-actions/import-codesign-certs@").includes("keychain: ${{ env.SIGNING_KEYCHAIN }}"),
  "the certificate must be imported into this run's keychain");
// The API key is a file only while this job runs; only the step that writes
// it sees its contents.
const writeKey = stepBlock(sign, "name: Write the notarization API key");
assert.match(writeKey, /umask 077/, "the API key file must be readable only by the job user");
assert.match(writeKey, /NOTARY_KEY_PATH=/, "the key path must be handed to the notarization steps");
assert.equal(sign.split("secrets.APPLE_NOTARY_KEY }}").length - 1, 1,
  "only the step writing the key file may receive the API key itself");
order("name: Write the notarization API key", "xcrun notarytool history");
for (const call of ["xcrun notarytool history", "xcrun notarytool submit"]) {
  const block = stepBlock(sign, call);
  assert.match(block, /--key "\$\{?NOTARY_KEY_PATH\}?"/, `${call} must use the API key file`);
  assert.match(block, /--key-id "\$\{?APPLE_NOTARY_KEY_ID\}?"/);
  assert.match(block, /--issuer "\$\{?APPLE_NOTARY_ISSUER_ID\}?"/);
}
const keychainCleanup = "name: Delete Apple signing credentials";
const cleanup = stepBlock(sign, keychainCleanup);
assert.match(cleanup, /^        if: always\(\)$/m, "the keychain must be deleted even when the job fails");
assert.ok(cleanup.includes("${SIGNING_KEYCHAIN}.keychain-db"), "the cleanup must delete the keychain this run imported into");
assert.ok(cleanup.includes("NOTARY_KEY_PATH"), "the cleanup must delete the API key file");
assert.equal(sign.indexOf("\n      - ", sign.indexOf(keychainCleanup)), -1,
  "the keychain cleanup must stay the final step");
assert.match(sign, /node scripts\/verify-migration-release\.mjs[\s\S]*?--distributed-assets/,
  "the signed app must be checked against signed migration provenance and packaged bytes");
assert.match(sign, /xcrun stapler staple/);
assert.match(sign, /xcrun stapler validate/);

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
assertPublishes("publish-macos", ["sign-macos"], ["*.dmg"]);

// Releases ship the Issue Tracker server only with --experimental-issues, so
// no release job builds it or caches its target directory.
for (const name of jobNames) {
  assert.doesNotMatch(jobBlock(workflow, name), /issue-tracker-server|--experimental-issues/,
    `job ${name} must not build or bundle the experimental Issue Tracker server`);
}

console.log("public release workflow checks passed");
