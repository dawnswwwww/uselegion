#!/usr/bin/env node
// `legion` entry point for the @uselegion/cli npm wrapper.
//
// Resolves the prebuilt `legion` binary that ships in the matching
// platform-specific optional dependency (@uselegion/cli-<plat>-<arch>) and
// spawns it with forwarded stdio + args. Falls back to a `legion` found on
// PATH if no platform package is installed (e.g. global installs where the
// optional dep was pruned).

const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");

// Map the current process to a platform package suffix.
const PLATFORMS = {
  "darwin-arm64": "@uselegion/cli-darwin-arm64",
  "darwin-x64": "@uselegion/cli-darwin-x64",
  "linux-arm64": "@uselegion/cli-linux-arm64-musl",
  "linux-x64": "@uselegion/cli-linux-x64-musl",
  "win32-x64": "@uselegion/cli-win32-x64-msvc",
};

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORMS[key];
  if (!pkg) {
    throw new Error(
      `Unsupported platform: ${key}. Supported: ${Object.keys(PLATFORMS).join(", ")}`,
    );
  }
  let pkgDir;
  try {
    pkgDir = require.resolve(`${pkg}/package.json`);
  } catch {
    // Optional dependency was pruned — fall back to PATH lookup.
    return null;
  }
  const dir = require("node:path").dirname(pkgDir);
  const exe =
    process.platform === "win32" ? "legion.exe" : "legion";
  // Platform packages ship the archive under bin/; we extract on install.
  const candidate = join(dir, "bin", exe);
  if (existsSync(candidate)) return candidate;
  return null;
}

function main() {
  let bin = resolveBinary();
  const args = process.argv.slice(2);

  if (!bin) {
    // Fall back to a `legion` on PATH (e.g. installed via cargo/brew).
    const child = spawn("legion", args, { stdio: "inherit" });
    child.on("error", (err) => {
      if (err.code === "ENOENT") {
        console.error(
          "legion: no platform binary installed and `legion` not found on PATH.\n" +
            "Install via Homebrew (`brew install dawnswwwww/tap/legion`) or Cargo (`cargo install legion-cli`).",
        );
        process.exit(127);
      }
      throw err;
    });
    child.on("exit", (code) => process.exit(code ?? 1));
    return;
  }

  const child = spawn(bin, args, { stdio: "inherit" });
  child.on("error", (err) => {
    console.error("legion: failed to spawn binary:", err.message);
    process.exit(1);
  });
  child.on("exit", (code) => process.exit(code ?? 1));
}

main();
