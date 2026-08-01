#!/usr/bin/env node
// Downloads the prebuilt pal binary for this platform from the matching
// GitHub release on first run, caches it, and execs it with the given args.
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

const pkg = require("../package.json");
const REPO = "JoeMattie/palimpsest";

function target() {
  const platforms = {
    "linux-x64": "x86_64-unknown-linux-musl",
    "linux-arm64": "aarch64-unknown-linux-musl",
    "darwin-x64": "x86_64-apple-darwin",
    "darwin-arm64": "aarch64-apple-darwin",
    "win32-x64": "x86_64-pc-windows-msvc",
  };
  const key = `${process.platform}-${process.arch}`;
  const t = platforms[key];
  if (!t) {
    console.error(
      `pal: no prebuilt binary for ${key}.\n` +
        `Build from source instead: cargo install --git https://github.com/${REPO} pal-cli`
    );
    process.exit(1);
  }
  return t;
}

function cacheDir() {
  const base =
    process.platform === "win32"
      ? process.env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local")
      : process.platform === "darwin"
        ? path.join(os.homedir(), "Library", "Caches")
        : process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
  return path.join(base, "palimpsest", pkg.version);
}

async function download(url, dest) {
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`download failed: ${res.status} ${res.statusText} for ${url}`);
  }
  const buf = Buffer.from(await res.arrayBuffer());
  fs.writeFileSync(dest, buf);
}

async function ensureBinary() {
  const dir = cacheDir();
  const bin = path.join(dir, process.platform === "win32" ? "pal.exe" : "pal");
  if (fs.existsSync(bin)) {
    return bin;
  }
  fs.mkdirSync(dir, { recursive: true });
  const t = target();
  const url = `https://github.com/${REPO}/releases/download/v${pkg.version}/pal-${t}.tar.gz`;
  const archive = path.join(dir, "pal.tar.gz");
  console.error(`pal: downloading ${url}`);
  await download(url, archive);
  const tar = spawnSync("tar", ["-xzf", archive, "-C", dir], { stdio: "inherit" });
  if (tar.status !== 0) {
    throw new Error("failed to extract archive (is tar available?)");
  }
  fs.unlinkSync(archive);
  if (process.platform !== "win32") {
    fs.chmodSync(bin, 0o755);
  }
  return bin;
}

ensureBinary()
  .then((bin) => {
    const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
    process.exit(result.status === null ? 1 : result.status);
  })
  .catch((err) => {
    console.error(`pal: ${err.message}`);
    process.exit(1);
  });
