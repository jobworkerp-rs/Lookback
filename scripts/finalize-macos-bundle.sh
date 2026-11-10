#!/usr/bin/env bash
# Finalize only an already assembled, provenance-verified Tauri .app.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
APP=""
SOURCE_ROOT=""
TRIPLE=""
DMG=""
UNSIGNED_INTERMEDIATE=0
NOTARIZE=0
WITH_ISSUE_TRACKER=0

usage() {
  cat >&2 <<'EOF'
Usage: scripts/finalize-macos-bundle.sh --app APP --source-root AGENT_APP --triple TARGET [options]

Options:
  --dmg FILE                  Create a signed DMG from the sealed .app
  --notarize                  Submit and staple the DMG using an existing
                              APPLE_NOTARY_KEYCHAIN_PROFILE
  --unsigned-intermediate     Validate an unsigned app without signing or DMG
                              creation; this output is not distributable
  --with-issue-tracker        The app ships issue-tracker-server (an
                              --experimental-issues release)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app|--source-root|--triple|--dmg)
      [[ $# -ge 2 && -n "$2" ]] || { usage; exit 2; }
      case "$1" in
        --app) APP=$2 ;;
        --source-root) SOURCE_ROOT=$2 ;;
        --triple) TRIPLE=$2 ;;
        --dmg) DMG=$2 ;;
      esac
      shift 2
      ;;
    --unsigned-intermediate) UNSIGNED_INTERMEDIATE=1; shift ;;
    --notarize) NOTARIZE=1; shift ;;
    --with-issue-tracker) WITH_ISSUE_TRACKER=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

[[ -n "${APP}" && -n "${SOURCE_ROOT}" && -n "${TRIPLE}" ]] \
  || { usage; exit 2; }
case "${TRIPLE}" in
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) echo "unsupported macOS release target: ${TRIPLE}" >&2; exit 2 ;;
esac
[[ -d "${APP}" && ! -L "${APP}" ]] \
  || { echo "assembled macOS .app is missing or not a real directory: ${APP}" >&2; exit 1; }
[[ -d "${SOURCE_ROOT}" && ! -L "${SOURCE_ROOT}" ]] \
  || { echo "trusted agent-app build root is missing or not a real directory" >&2; exit 1; }
APP=$(cd "${APP}" && pwd -P)
SOURCE_ROOT=$(cd "${SOURCE_ROOT}" && pwd -P)

# The externalBin sidecars Tauri copies into Contents/MacOS. They must match
# the trusted build inputs, are the only expected Mach-O files there, and are
# all required.
EXTERNAL_BINS=(all-in-one front conductor-main memories-import protoc)
[[ "${WITH_ISSUE_TRACKER}" == "0" ]] || EXTERNAL_BINS+=(issue-tracker-server)

if [[ "${UNSIGNED_INTERMEDIATE}" == "1" ]]; then
  [[ -z "${DMG}" && "${NOTARIZE}" == "0" ]] \
    || { echo "unsigned intermediate mode only permits an app; it cannot create or notarize a DMG" >&2; exit 1; }
else
  [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]] \
    || { echo "APPLE_SIGNING_IDENTITY is required for macOS release finalization" >&2; exit 1; }
fi
if [[ "${NOTARIZE}" == "1" ]]; then
  [[ -n "${DMG}" ]] || { echo "--notarize requires --dmg" >&2; exit 2; }
  [[ -n "${APPLE_NOTARY_KEYCHAIN_PROFILE:-}" ]] \
    || { echo "APPLE_NOTARY_KEYCHAIN_PROFILE is required for notarization" >&2; exit 1; }
fi

require_cmd() {
  local command
  for command in "$@"; do
    command -v "${command}" >/dev/null 2>&1 \
      || { echo "required macOS finalization tool is missing: ${command}" >&2; exit 1; }
  done
}

require_cmd node file
if [[ "${UNSIGNED_INTERMEDIATE}" != "1" ]]; then
  require_cmd codesign
fi
if [[ -n "${DMG}" ]]; then
  require_cmd hdiutil
fi
if [[ "${NOTARIZE}" == "1" ]]; then
  require_cmd xcrun
fi

# Framework bundles commonly contain relative symlinks, but every app symlink
# must resolve to an object within this app. Resolve each link without walking
# through it so external, broken, and cyclic targets fail before inspection or
# signing can act on a redirected path.
node - "${APP}" <<'NODE'
const fs = require("node:fs");
const path = require("node:path");

const appRoot = fs.realpathSync(process.argv[2]);
function fail(message) { throw new Error(message); }
function isWithinApp(target) {
  const relative = path.relative(appRoot, target);
  return relative === "" ||
    (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
}
function inspect(entry) {
  const stat = fs.lstatSync(entry);
  if (stat.isSymbolicLink()) {
    let target;
    try { target = fs.realpathSync(entry); }
    catch { fail(`app symlink is broken or cyclic: ${entry}`); }
    if (!isWithinApp(target)) fail(`app symlink escapes the bundle: ${entry}`);
    return;
  }
  if (stat.isDirectory()) {
    for (const name of fs.readdirSync(entry)) inspect(path.join(entry, name));
  }
}
inspect(appRoot);
NODE

case "${TRIPLE}" in
  *-apple-darwin) ATLAS_PLATFORM=darwin-arm64 ;;
esac
SOURCE_BUNDLE="${SOURCE_ROOT}/src-tauri/migration-bundle"
PACKAGED_RESOURCES="${APP}/Contents/Resources"
PACKAGED_BUNDLE="${PACKAGED_RESOURCES}/memories-db-migrate"
FRONT_SOURCE="${SOURCE_ROOT}/src-tauri/bin/front-${TRIPLE}"
IMPORT_SOURCE="${SOURCE_ROOT}/src-tauri/bin/memories-import-${TRIPLE}"
FRONT_PACKAGED="${APP}/Contents/MacOS/front"
IMPORT_PACKAGED="${APP}/Contents/MacOS/memories-import"

node "${SCRIPT_DIR}/verify-migration-release.mjs" "${SOURCE_BUNDLE}" \
  --require-provenance --source-artifacts --atlas-platform "${ATLAS_PLATFORM}" \
  --front "${FRONT_SOURCE}" --import "${IMPORT_SOURCE}" \
  --app-assets "${SOURCE_ROOT}" \
  || { echo "trusted migration build inputs failed source provenance verification" >&2; exit 1; }
node "${SCRIPT_DIR}/verify-atlas-lock.mjs" "${SOURCE_BUNDLE}/atlas" "${ATLAS_PLATFORM}" \
  || { echo "trusted upstream Atlas binary failed its source lock" >&2; exit 1; }

# Tauri must have copied these inputs exactly. In particular, sidecars and Atlas
# must remain unsigned until this check has established their source hashes.
EXTERNAL_BIN_NAMES="${EXTERNAL_BINS[*]}" \
  node --input-type=module - "${SOURCE_ROOT}" "${APP}" "${TRIPLE}" <<'NODE'
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";

const [sourceRoot, appRoot, triple] = process.argv.slice(2);
const resources = path.join(appRoot, "Contents", "Resources");
const mainInfo = path.join(appRoot, "Contents", "Info.plist");
function fail(message) { throw new Error(message); }
function fileDigest(file, label) {
  let stat;
  try { stat = fs.lstatSync(file); } catch { fail(`${label} is missing`); }
  if (!stat.isFile() || stat.isSymbolicLink()) fail(`${label} is not a regular file`);
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}
function walk(root, relative = "") {
  let stat;
  try { stat = fs.lstatSync(root); }
  catch (error) { if (error.code === "ENOENT") return []; throw error; }
  if (stat.isSymbolicLink()) fail(`trusted resource contains a symlink: ${root}`);
  if (stat.isFile()) return [relative];
  if (!stat.isDirectory()) fail(`trusted resource contains a non-file path: ${root}`);
  return fs.readdirSync(root).sort().flatMap((name) =>
    walk(path.join(root, name), relative ? `${relative}/${name}` : name));
}
function compareTree(source, packaged, label) {
  const sourceFiles = walk(source);
  const packagedFiles = walk(packaged);
  if (JSON.stringify(sourceFiles) !== JSON.stringify(packagedFiles)) {
    fail(`${label} does not match trusted build input (file list differs)`);
  }
  for (const relative of sourceFiles) {
    if (fileDigest(path.join(source, relative), `${label} source`) !==
        fileDigest(path.join(packaged, relative), `${label} package`)) {
      fail(`${label} does not match trusted build input: ${relative}`);
    }
  }
}
function compareFile(source, packaged, label) {
  if (fileDigest(source, `${label} source`) !== fileDigest(packaged, `${label} package`)) {
    fail(`${label} does not match trusted build input`);
  }
}

compareTree(path.join(sourceRoot, "src-tauri", "migration-bundle"),
  path.join(resources, "memories-db-migrate"), "migration bundle");
compareTree(path.join(sourceRoot, "src-tauri", "plugins"),
  path.join(resources, "plugins"), "plugin resources");
compareTree(path.join(sourceRoot, "workers"), path.join(resources, "workers"), "worker resources");
compareTree(path.join(sourceRoot, "dict"), path.join(resources, "dict"), "dictionary resources");
compareTree(path.join(sourceRoot, "resources", "coding-agent"),
  path.join(resources, "coding-agent"), "coding-agent resources");

for (const name of process.env.EXTERNAL_BIN_NAMES.split(" ")) {
  compareFile(path.join(sourceRoot, "src-tauri", "bin", `${name}-${triple}`),
    path.join(appRoot, "Contents", "MacOS", name), `external binary ${name}`);
}

let info;
try { info = fs.readFileSync(mainInfo, "utf8"); } catch { fail("assembled app Info.plist is missing"); }
const executable = info.match(/<key>CFBundleExecutable<\/key>\s*<string>([^<]+)<\/string>/)?.[1];
if (!executable || !/^[A-Za-z0-9._-]+$/.test(executable)) fail("app Info.plist has an invalid CFBundleExecutable");
const mainExecutable = path.join(appRoot, "Contents", "MacOS", executable);
fileDigest(mainExecutable, "app main executable");
NODE

node "${SCRIPT_DIR}/verify-migration-release.mjs" "${PACKAGED_BUNDLE}" \
  --require-provenance --source-artifacts --atlas-platform "${ATLAS_PLATFORM}" \
  --front "${FRONT_PACKAGED}" --import "${IMPORT_PACKAGED}" \
  --app-assets "${SOURCE_ROOT}" \
  || { echo "Tauri assembled migration assets do not match the trusted source build" >&2; exit 1; }

MAIN_EXECUTABLE=$(node - "${APP}/Contents/Info.plist" <<'NODE'
import fs from "node:fs";
const text = fs.readFileSync(process.argv[2], "utf8");
const value = text.match(/<key>CFBundleExecutable<\/key>\s*<string>([^<]+)<\/string>/)?.[1];
if (!value || !/^[A-Za-z0-9._-]+$/.test(value)) process.exit(1);
process.stdout.write(value);
NODE
)
MAIN_PATH="${APP}/Contents/MacOS/${MAIN_EXECUTABLE}"
is_external_bin() {
  local name
  for name in "${EXTERNAL_BINS[@]}"; do
    [[ "$1" == "${name}" ]] && return 0
  done
  return 1
}
MACH_O_FILES=()
SEEN_MAIN=0
while IFS= read -r -d '' candidate; do
  description=$(file -b "${candidate}" 2>/dev/null || true)
  [[ "${description}" == *"Mach-O"* ]] || continue
  relative=${candidate#"${APP}/"}
  case "${relative}" in
    "Contents/MacOS/${MAIN_EXECUTABLE}") SEEN_MAIN=1 ;;
    Contents/MacOS/*) is_external_bin "${relative#Contents/MacOS/}" \
      || { echo "unexpected Mach-O artifact in assembled app: ${relative}" >&2; exit 1; } ;;
    Contents/Resources/plugins/*.dylib) ;;
    Contents/Resources/memories-db-migrate/memories-db-migrate|Contents/Resources/memories-db-migrate/atlas/bin/atlas) ;;
    *) echo "unexpected Mach-O artifact in assembled app: ${relative}" >&2; exit 1 ;;
  esac
  MACH_O_FILES+=("${candidate}")
done < <(find "${APP}" -type f -print0)

[[ "${SEEN_MAIN}" == "1" ]] \
  || { echo "assembled app main executable is not a Mach-O file" >&2; exit 1; }
for required in "${EXTERNAL_BINS[@]}"; do
  found=0
  for candidate in "${MACH_O_FILES[@]}"; do
    [[ "${candidate}" == "${APP}/Contents/MacOS/${required}" ]] && found=1
  done
  [[ "${found}" == "1" ]] \
    || { echo "expected external binary is missing or is not Mach-O: ${required}" >&2; exit 1; }
done
for required in \
  "${PACKAGED_BUNDLE}/memories-db-migrate" \
  "${PACKAGED_BUNDLE}/atlas/bin/atlas"; do
  found=0
  for candidate in "${MACH_O_FILES[@]}"; do
    [[ "${candidate}" == "${required}" ]] && found=1
  done
  [[ "${found}" == "1" ]] \
    || { echo "expected migration executable is missing or is not Mach-O: ${required}" >&2; exit 1; }
done

if [[ "${UNSIGNED_INTERMEDIATE}" == "1" ]]; then
  echo "unsigned intermediate macOS .app verified; it is not a distributable release"
  exit 0
fi

ENTITLEMENTS="${SOURCE_ROOT}/src-tauri/entitlements.plist"
[[ -f "${ENTITLEMENTS}" && ! -L "${ENTITLEMENTS}" ]] \
  || { echo "macOS hardened-runtime entitlements are missing: src-tauri/entitlements.plist" >&2; exit 1; }
IDENTITY=${APPLE_SIGNING_IDENTITY}
echo "Signing nested macOS Mach-O files in the verified app"
for candidate in "${MACH_O_FILES[@]}"; do
  [[ "${candidate}" == "${MAIN_PATH}" ]] && continue
  codesign --force --options runtime --timestamp --sign "${IDENTITY}" "${candidate}"
  codesign --verify --strict --verbose=2 "${candidate}"
done

codesign --force --options runtime --timestamp --entitlements "${ENTITLEMENTS}" \
  --sign "${IDENTITY}" "${MAIN_PATH}"
codesign --verify --strict --verbose=2 "${MAIN_PATH}"

node "${SCRIPT_DIR}/record-signed-atlas-lock.mjs" \
  "${PACKAGED_BUNDLE}/atlas" "${ATLAS_PLATFORM}"
node "${SCRIPT_DIR}/lib/record-signed-migration-provenance.mjs" \
  "${PACKAGED_BUNDLE}" "${FRONT_PACKAGED}" "${IMPORT_PACKAGED}" \
  front memories_import memories_db_migrate
node "${SCRIPT_DIR}/verify-migration-release.mjs" "${PACKAGED_BUNDLE}" \
  --require-provenance --atlas-platform "${ATLAS_PLATFORM}" \
  --app-assets "${SOURCE_ROOT}" --distributed-assets "${PACKAGED_RESOURCES}" \
  --front "${FRONT_PACKAGED}" --import "${IMPORT_PACKAGED}" \
  || { echo "signed macOS app failed final migration provenance verification" >&2; exit 1; }

codesign --force --options runtime --timestamp --entitlements "${ENTITLEMENTS}" \
  --sign "${IDENTITY}" "${APP}"
codesign --verify --deep --strict --verbose=2 "${APP}"
node "${SCRIPT_DIR}/verify-migration-release.mjs" "${PACKAGED_BUNDLE}" \
  --require-provenance --atlas-platform "${ATLAS_PLATFORM}" \
  --app-assets "${SOURCE_ROOT}" --distributed-assets "${PACKAGED_RESOURCES}" \
  --front "${FRONT_PACKAGED}" --import "${IMPORT_PACKAGED}" \
  || { echo "sealed macOS app failed final migration provenance verification" >&2; exit 1; }

if [[ -n "${DMG}" ]]; then
  mkdir -p "$(dirname "${DMG}")"
  hdiutil create -volname "Lookback" -srcfolder "${APP}" -ov -format UDZO "${DMG}" >/dev/null
  codesign --force --timestamp --sign "${IDENTITY}" "${DMG}"
  codesign --verify --verbose=2 "${DMG}"
  if [[ "${NOTARIZE}" == "1" ]]; then
    echo "Submitting signed macOS DMG for notarization"
    xcrun notarytool submit "${DMG}" \
      --keychain-profile "${APPLE_NOTARY_KEYCHAIN_PROFILE}" --wait --output-format json >/dev/null
    xcrun stapler staple "${DMG}" >/dev/null
    xcrun stapler validate "${DMG}" >/dev/null
  fi
fi

echo "macOS bundle finalized and verified: ${APP}"
