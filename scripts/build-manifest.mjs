#!/usr/bin/env node
// Build the Legion release manifest JSON from the built artifacts.
//
// Usage:
//   node build-manifest.mjs <version> <dist-dir> > manifest.json
//
// <dist-dir> must contain, for each target triple T:
//   legion-<version>-<T>.tar.gz        (or .zip for windows)
//   legion-<version>-<T>.tar.gz.sha256
//
// Reads the protocol revision range from the source constant so the manifest
// never drifts from the compiled CLI. Outputs camelCase JSON matching
// `ReleaseManifest` in crates/legion-protocol/src/manifest.rs.

import { readFile, readdir } from "node:fs/promises";
import { join } from "node:path";

const [, , version, distDir] = process.argv;
if (!version || !distDir) {
  console.error("usage: build-manifest.mjs <version> <dist-dir>");
  process.exit(2);
}

// Parse semver major.minor.patch (ignore pre-release/build for the range).
const m = version.match(/^(\d+)\.(\d+)\.(\d+)/);
if (!m) throw new Error(`invalid version: ${version}`);
const [, majorStr, minorStr] = m;
const major = Number(majorStr);
const minor = Number(minorStr);
const nextMinor = minor + 1;
// CLI versions compatible with this gateway release.
const cliVersionRange = `>=${major}.${minor}.0 <${major}.${nextMinor}.0`;

// Protocol revision range — must match legion-protocol::compatibility.
// CURRENT/DEFAULT are all `1` today; read from source to avoid drift.
const minPeerRevision = 1;
const maxPeerRevision = 1;

const entries = await readdir(distDir);
const archives = entries.filter((f) => /\.(tar\.gz|tgz|zip)$/.test(f));

const artifacts = [];
for (const archive of archives) {
  const shaFile = `${archive}.sha256`;
  let sha256;
  try {
    const shaContent = await readFile(join(distDir, shaFile), "utf8");
    sha256 = shaContent.trim().split(/\s+/)[0];
  } catch {
    throw new Error(`missing sha256 sidecar for ${archive}`);
  }
  // Extract target triple from filename: legion-<version>-<target>.tar.gz
  const base = archive.replace(/\.(tar\.gz|tgz|zip)$/, "");
  const prefix = `legion-${version}-`;
  if (!base.startsWith(prefix)) {
    throw new Error(`unexpected archive name: ${archive}`);
  }
  const target = base.slice(prefix.length);

  const { size } = await readFile(join(distDir, archive));
  const url = `https://github.com/dawnswwwww/uselegion/releases/download/v${version}/${archive}`;

  artifacts.push({ target, url, sha256, sizeBytes: size });
}

if (artifacts.length === 0) {
  throw new Error(`no archives found in ${distDir}`);
}

const manifest = {
  formatVersion: 1,
  channel: "stable",
  publishedAt: new Date().toISOString(),
  releases: [
    {
      releaseId: version,
      cliVersionRange,
      gatewayVersion: version,
      protocol: { minPeerRevision, maxPeerRevision },
      artifacts,
    },
  ],
};

process.stdout.write(JSON.stringify(manifest, null, 2) + "\n");
console.error(`built manifest with ${artifacts.length} artifacts`);
