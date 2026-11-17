// Replaces bin/loch, a Node shim, with the native binary itself, so that a
// `loch` on PATH from `npm install -g` costs nothing but loch. Hardlinked
// where possible and copied where not. Windows keeps the shim: npm reaches
// bins there through a .cmd wrapper that runs Node either way.
//
// If this never runs (--ignore-scripts), the shim still works, just slower.
"use strict";

const fs = require("fs");
const path = require("path");
const { binaryPath } = require("./index.js");

if (process.platform !== "win32") {
  try {
    const native = binaryPath();
    const target = path.join(__dirname, "bin", "loch");
    const staged = `${target}.native`;
    fs.rmSync(staged, { force: true });
    try {
      fs.linkSync(native, staged);
    } catch {
      fs.copyFileSync(native, staged);
    }
    fs.chmodSync(staged, 0o755);
    fs.renameSync(staged, target);
  } catch (error) {
    process.stderr.write(`loch: keeping the Node shim (${error.message})\n`);
  }
}
