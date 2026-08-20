#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const binary = path.join(
  __dirname,
  "..",
  "vendor",
  process.platform === "win32" ? "haider.exe" : "haider"
);

if (!fs.existsSync(binary)) {
  console.error("haider binary is missing. Reinstall with: npm i -g haider-agent --force");
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`Failed to run haider: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
