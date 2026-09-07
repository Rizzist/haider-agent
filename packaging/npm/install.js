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
    "darwin-arm64": `haider-v${VERSION}-aarch64-apple-darwin-split.tar.xz`,
    "darwin-x64": `haider-v${VERSION}-x86_64-apple-darwin-split.tar.xz`,
    "linux-x64": `haider-v${VERSION}-x86_64-unknown-linux-gnu-split.tar.xz`,
    "linux-arm64": `haider-v${VERSION}-aarch64-unknown-linux-gnu-split.tar.xz`,
    "win32-x64": `haider-v${VERSION}-x86_64-pc-windows-msvc-split.zip`
  };
  return artifacts[key] || null;
}

// Match install.sh and the shared updater capacities. A 1 MiB/s transfer
// floor plus connection setup gives each attempt its own finite wall budget;
// redirects/body trickles never restart it or consume the installer watchdog.
const FETCH_ATTEMPTS = 2;
const FETCH_CONNECT_SECONDS = 30;
const FETCH_TRANSFER_BYTES_PER_SECOND = 1024 * 1024;
const FETCH_ARCHIVE_BYTES = 128 * 1024 * 1024;
const FETCH_CHECKSUM_BYTES = 16 * 1024;
function downloadAttemptMs(maxBytes) {
  return (FETCH_CONNECT_SECONDS + Math.ceil(maxBytes / FETCH_TRANSFER_BYTES_PER_SECOND)) * 1000;
}

async function download(url, {
  maxBytes = FETCH_ARCHIVE_BYTES,
  attemptMs = downloadAttemptMs(maxBytes),
  get = https.get,
  // Tests can expire a deadline after observing real network events.
  setTimeout: scheduleTimeout = setTimeout,
  clearTimeout: cancelTimeout = clearTimeout
} = {}) {
  let lastError;
  for (let attempt = 0; attempt < FETCH_ATTEMPTS; attempt++) {
    try {
      return await new Promise((resolve, reject) => {
        let activeRequest;
        let activeResponse;
        const resources = new Set();
        let settled = false;
        const finish = (error, value) => {
          if (settled) return;
          settled = true;
          cancelTimeout(timer);
          // Includes every redirect body, even when it never ends. Replacing
          // only the latest request would leave earlier sockets alive.
          for (const resource of resources) resource.destroy();
          if (error) reject(error);
          else resolve(value);
        };
        const timer = scheduleTimeout(() => finish(new Error(`Download attempt timed out for ${url}`)), attemptMs);
        const visit = (currentUrl, redirects) => {
          if (settled) return;
          try {
            activeRequest = get(currentUrl, {
              headers: { "User-Agent": `HaiderNpmInstaller/${VERSION}` }
            }, (response) => {
              activeResponse = response;
              resources.add(activeResponse);
              response.on("error", (error) => finish(error));
              response.on("aborted", () => finish(new Error(`Download aborted for ${currentUrl}`)));
              const location = response.headers.location;
              if (response.statusCode >= 300 && response.statusCode < 400 && location && redirects < 5) {
                response.resume();
                visit(new URL(location, currentUrl).toString(), redirects + 1);
                return;
              }
              if (response.statusCode !== 200) {
                finish(new Error(`HTTP ${response.statusCode} for ${currentUrl}`));
                return;
              }
              const chunks = [];
              response.on("data", (chunk) => chunks.push(chunk));
              response.on("end", () => finish(null, Buffer.concat(chunks)));
            });
            resources.add(activeRequest);
            activeRequest.on("error", (error) => finish(error));
          } catch (error) { finish(error); }
        };
        visit(url, 0);
      });
    } catch (error) { lastError = error; }
  }
  throw lastError;
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
  const wanted = new Set(["haider.exe", "haider-tui.exe", "haiderd.exe"]);
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
      if (extracted.has(basename)) {
        throw new Error(`Archive contains duplicate ${basename}`);
      }
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
    const binaries = ["haider", "haider-tui", "haiderd"];
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

// Archive checksum validation precedes this call. The extracted thin binary
// owns all publication, locking, verification and durable recovery. Never erase
// vendor or its recovery marker when the helper refuses or is interrupted.
function installArchive(archive, artifact, vendorDir, runInstaller = spawnSync) {
  const stage = fs.mkdtempSync(path.join(os.tmpdir(), "haider-npm-stage-"));
  try {
    if (artifact.endsWith(".zip")) {
      extractZipBinaries(archive, stage);
    } else {
      extractTarXzBinaries(archive, artifact, stage);
    }
    for (const binary of fs.readdirSync(stage)) {
      fs.chmodSync(path.join(stage, binary), 0o755);
    }
    const binary = path.join(stage, artifact.endsWith(".zip") ? "haider.exe" : "haider");
    const result = runInstaller(binary, ["--install-bundle", stage, vendorDir], { stdio: "inherit" });
    if (result.error) throw new Error(`Could not start bundle installer: ${result.error.message}`);
    if (result.status !== 0) throw new Error(`Bundle installation failed with exit code ${result.status}`);
  } finally {
    fs.rmSync(stage, { recursive: true, force: true });
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
    download(sidecarUrl, { maxBytes: FETCH_CHECKSUM_BYTES })
  ]);
  const expected = expectedHash(sidecar, artifact);
  if (!expected) {
    throw new Error(`${artifact}.sha256 did not contain a valid checksum`);
  }
  const actual = sha256(archive);
  if (actual !== expected) {
    throw new Error(`Checksum mismatch for ${artifact}: expected ${expected}, got ${actual}`);
  }

  installArchive(archive, artifact, VENDOR_DIR);
  console.log(`Installed Haider binaries to ${VENDOR_DIR}`);
}

if (require.main === module) {
  main().catch((error) => {
    console.error(`Failed to install haider: ${error.message}`);
    console.error(
      "GitHub releases are public and do not require GITHUB_TOKEN. " +
        "This installer does not implement proxy support; configure direct HTTPS access or install manually."
    );
    process.exit(1);
  });
}

module.exports = {
  download,
  downloadAttemptMs,
  installArchive,
  artifactForCurrentPlatform,
  expectedHash,
  extractTarXzBinaries,
  extractZipBinaries
};
