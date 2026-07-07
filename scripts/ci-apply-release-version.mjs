#!/usr/bin/env node
import { readFileSync, writeFileSync } from "node:fs";

const [, , rawTag, configPath = "src-tauri/tauri.conf.json"] = process.argv;

if (!rawTag) {
  console.error("usage: ci-apply-release-version.mjs <tag> [tauri-conf]");
  process.exit(2);
}

const version = rawTag.replace(/^v/, "");
const semverPattern =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

if (!semverPattern.test(version)) {
  console.error(`release tag must be a semver version, got: ${rawTag}`);
  process.exit(1);
}

const config = JSON.parse(readFileSync(configPath, "utf8"));
config.version = version;

// Tauri reads this field for bundle names and package metadata.
writeFileSync(configPath, `${JSON.stringify(config, null, 2)}\n`);
