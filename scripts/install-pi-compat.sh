#!/usr/bin/env bash
# Install one pi release into extension/node_modules for a compat run, without
# touching the development pin or lockfile. Run from extension/.
#   install-pi-compat.sh <pi-version> [extra npm specs...]
# Extra specs are the pins from .github/pi-compat.json; upstream-drift passes none.
set -euo pipefail
version=$1
shift

# Resolve latest once, then install that exact release and assert it.
resolved=$(npm view "@earendil-works/pi-coding-agent@$version" version)
npm install --no-save --package-lock=false "@earendil-works/pi-coding-agent@$resolved" "$@"
PI_EXPECTED_VERSION="$resolved" node --input-type=commonjs -e '
  const actual = require("./node_modules/@earendil-works/pi-coding-agent/package.json").version;
  const expected = process.env.PI_EXPECTED_VERSION;
  if (actual !== expected) throw new Error(`Expected pi ${expected}, installed ${actual}`);
  console.log(`Verified pi ${actual}`);
'
# Show what pi's own @earendil-works ranges resolved to, so a drift failure
# names the upstream versions.
# Read package.json files directly: pi's exports map does not expose them.
node --input-type=commonjs -e '
  const { existsSync, readFileSync } = require("node:fs");
  const pi = "node_modules/@earendil-works/pi-coding-agent";
  const { dependencies = {} } = JSON.parse(readFileSync(`${pi}/package.json`, "utf8"));
  for (const [name, range] of Object.entries(dependencies)) {
    if (!name.startsWith("@earendil-works/")) continue;
    const found = [`${pi}/node_modules/${name}`, `node_modules/${name}`]
      .map(dir => `${dir}/package.json`).find(existsSync);
    const version = found ? JSON.parse(readFileSync(found, "utf8")).version : "missing";
    console.log(`  ${name} ${range} -> ${version}`);
  }
'
git diff --exit-code -- package.json package-lock.json
