// Where this platform's loch binary is, for programs that embed loch (Ness,
// the VS Code extension). Spawn the path this returns rather than the `loch`
// on PATH or this package's bin/loch: the hooks run twice per agent shell
// command, and going through Node first would put its startup back on every
// call. Electron apps must also asarUnpack "**/node_modules/@ness-dev/loch-*/**"
// — a native binary can't be executed from inside an asar archive.
"use strict";

const { dirname, join, sep } = require("path");
const platforms = require("./platforms.json");

function platformEntry() {
  const key = `${process.platform}-${process.arch}`;
  // Linux builds are static musl binaries, so one package per architecture
  // serves glibc and musl alike — unlike Claude Code, there is no -musl split.
  const entry = Object.values(platforms).find((p) => `${p.os}-${p.cpu}` === key);
  if (!entry) throw new Error(`loch has no build for ${key}`);
  return entry;
}

function binaryPath() {
  const entry = platformEntry();
  const manifest = require.resolve(`${entry.package}/package.json`);
  return join(dirname(manifest), entry.bin).replace(
    `${sep}app.asar${sep}`,
    `${sep}app.asar.unpacked${sep}`,
  );
}

module.exports = { binaryPath, platformEntry };
