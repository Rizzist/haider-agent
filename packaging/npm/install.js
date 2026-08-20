#!/usr/bin/env node
"use strict";

const crypto = require("crypto");
const fs = require("fs");
const https = require("https");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");
const zlib = require("zlib");

const pkg = require("./package.json");

const ROOT = __dirname;
const VENDOR_DIR = path.join(ROOT, "vendor");
const VERSION = pkg.version.replace(/^v/, "");
const RELEASE = `https://github.com/Rizzist/haider-agent/releases/download/v${VERSION}`;

function artifactForCurrentPlatform() {
  const key = `${process.platform}-${process.arch}`;
  const artifacts = {
    "darwin-arm64": `haider-v${VERSION}-aarch64-apple-darwin.tar.xz`,
    "darwin-x64": `haider-v${VERSION}-x86_64-apple-darwin.tar.xz`,
    "linux-x64": `haider-v${VERSION}-x86_64-unknown-linux-gnu.tar.xz`,
    "linux-arm64": `haider-v${VERSION}-aarch64-unknown-linux-gnu.tar.xz`,
    "win32-x64": `haider-v${VERSION}-x86_64-pc-windows-msvc.zip`
  };
  return artifacts[key] || null;
}

function download(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    const request = https.get(
      url,
      { headers: { "User-Agent": `HaiderNpmInstaller/${VERSION}` } },
      (response) => {
        const location = response.headers.location;
        if (
          response.statusCode >= 300 &&
          response.statusCode < 400 &&
          location &&
          redirects < 5
        ) {
          response.resume();
          resolve(download(new URL(location, url).toString(), redirects + 1));
          return;
        }

        if (response.statusCode !== 200) {
          response.resume();
          reject(new Error(`HTTP ${response.statusCode} for ${url}`));
          return;
        }

        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => resolve(Buffer.concat(chunks)));
      }
    );
    request.on("error", reject);
  });
}

function sha256(buffer) {
  return crypto.createHash("sha256").update(buffer).digest("hex");
}

function expectedHash(sidecarBuffer, artifact) {
  const lines = sidecarBuffer.toString("utf8").split(/\r?\n/);
  for (const line of lines) {
    const parts = line.trim().split(/\s+/);
    if (!/^[a-f0-9]{64}$/i.test(parts[0] || "")) {
      continue;
    }
    if (parts.length === 1) {
      return parts[0].toLowerCase();
    }
    const file = parts[1].replace(/^\*/, "").replace(/^\.\//, "");
    const basename = file.replace(/\\/g, "/").split("/").pop();
    if (basename === artifact) {
      return parts[0].toLowerCase();
    }
  }
  return null;
}

function findEndOfCentralDirectory(buffer) {
  const min = Math.max(0, buffer.length - 65557);
  for (let offset = buffer.length - 22; offset >= min; offset--) {
    if (buffer.readUInt32LE(offset) === 0x06054b50) {
      return offset;
    }
  }
  throw new Error("Invalid zip: end of central directory not found");
}

function extractZipBinaries(archiveBuffer, destDir) {
  const wanted = new Set(["haider.exe", "haiderd.exe"]);
  const extracted = new Set();
  const eocd = findEndOfCentralDirectory(archiveBuffer);
  const entries = archiveBuffer.readUInt16LE(eocd + 10);
  let centralOffset = archiveBuffer.readUInt32LE(eocd + 16);

  for (let index = 0; index < entries; index++) {
    if (archiveBuffer.readUInt32LE(centralOffset) !== 0x02014b50) {
      throw new Error("Invalid zip: central directory header not found");
    }

    const method = archiveBuffer.readUInt16LE(centralOffset + 10);
    const compressedSize = archiveBuffer.readUInt32LE(centralOffset + 20);
    const uncompressedSize = archiveBuffer.readUInt32LE(centralOffset + 24);
    const nameLength = archiveBuffer.readUInt16LE(centralOffset + 28);
    const extraLength = archiveBuffer.readUInt16LE(centralOffset + 30);
    const commentLength = archiveBuffer.readUInt16LE(centralOffset + 32);
    const localOffset = archiveBuffer.readUInt32LE(centralOffset + 42);
    const nameStart = centralOffset + 46;
    const name = archiveBuffer
      .subarray(nameStart, nameStart + nameLength)
      .toString("utf8");
    const basename = name.replace(/\\/g, "/").split("/").pop();

    if (wanted.has(basename)) {
      if (archiveBuffer.readUInt32LE(localOffset) !== 0x04034b50) {
        throw new Error("Invalid zip: local file header not found");
      }
      const localNameLength = archiveBuffer.readUInt16LE(localOffset + 26);
      const localExtraLength = archiveBuffer.readUInt16LE(localOffset + 28);
      const dataStart = localOffset + 30 + localNameLength + localExtraLength;
      const compressed = archiveBuffer.subarray(dataStart, dataStart + compressedSize);
      let file;
      if (method === 0) {
        file = compressed;
      } else if (method === 8) {
        file = zlib.inflateRawSync(compressed);
      } else {
        throw new Error(`Unsupported zip compression method ${method}`);
      }
      if (file.length !== uncompressedSize) {
        throw new Error(`Invalid zip size for ${basename}`);
      }
      fs.writeFileSync(path.join(destDir, basename), file);
      extracted.add(basename);
    }

    centralOffset = nameStart + nameLength + extraLength + commentLength;
  }

  for (const binary of wanted) {
    if (!extracted.has(binary)) {
      throw new Error(`Archive did not contain ${binary}`);
    }
  }
}

function extractTarXzBinaries(archiveBuffer, artifact, destDir) {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "haider-npm-"));
  try {
    const archivePath = path.join(tempDir, artifact);
    const unpackDir = path.join(tempDir, "unpack");
    fs.writeFileSync(archivePath, archiveBuffer);
    fs.mkdirSync(unpackDir);

    const result = spawnSync("tar", ["-xJf", archivePath, "-C", unpackDir], {
      encoding: "utf8"
    });
    if (result.error) {
      throw new Error(`Could not run tar: ${result.error.message}`);
    }
    if (result.status !== 0) {
      const detail = (result.stderr || result.stdout || "unknown error").trim();
      throw new Error(`Could not extract ${artifact}: ${detail}`);
    }

    const bundleDir = path.join(unpackDir, artifact.slice(0, -".tar.xz".length));
    const binaries = ["haider", "haiderd"];
    if (process.platform === "linux") {
      binaries.push("haider-wayland-portal");
    }
    for (const binary of binaries) {
      const source = path.join(bundleDir, binary);
      if (!fs.existsSync(source)) {
        throw new Error(`Archive did not contain ${binary}`);
      }
      fs.copyFileSync(source, path.join(destDir, binary));
    }
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
}

async function main() {
  const artifact = artifactForCurrentPlatform();
  if (!artifact) {
    throw new Error(
      `Unsupported platform ${process.platform}/${process.arch}. ` +
        "Install from https://github.com/Rizzist/haider-agent/releases instead."
    );
  }

  const artifactUrl = `${RELEASE}/${artifact}`;
  const sidecarUrl = `${artifactUrl}.sha256`;
  console.log(`Downloading ${artifact}`);

  const [archive, sidecar] = await Promise.all([
    download(artifactUrl),
    download(sidecarUrl)
  ]);
  const expected = expectedHash(sidecar, artifact);
  if (!expected) {
    throw new Error(`${artifact}.sha256 did not contain a valid checksum`);
  }
  const actual = sha256(archive);
  if (actual !== expected) {
    throw new Error(`Checksum mismatch for ${artifact}: expected ${expected}, got ${actual}`);
  }

  fs.rmSync(VENDOR_DIR, { recursive: true, force: true });
  fs.mkdirSync(VENDOR_DIR, { recursive: true });

  if (artifact.endsWith(".zip")) {
    extractZipBinaries(archive, VENDOR_DIR);
  } else {
    extractTarXzBinaries(archive, artifact, VENDOR_DIR);
  }

  for (const binary of fs.readdirSync(VENDOR_DIR)) {
    fs.chmodSync(path.join(VENDOR_DIR, binary), 0o755);
  }
  console.log(`Installed Haider binaries to ${VENDOR_DIR}`);
}

if (require.main === module) {
  main().catch((error) => {
    fs.rmSync(VENDOR_DIR, { recursive: true, force: true });
    console.error(`Failed to install haider: ${error.message}`);
    console.error(
      "GitHub releases are public and do not require GITHUB_TOKEN. " +
        "This installer does not implement proxy support; configure direct HTTPS access or install manually."
    );
    process.exit(1);
  });
}

module.exports = {
  expectedHash,
  extractTarXzBinaries,
  extractZipBinaries
};
