// Lays out the npm packages for one release from built binaries:
//
//   node npm/assemble.mjs <version> <out-dir> <target>=<binary> [...]
//
// e.g. aarch64-apple-darwin=rust/target/aarch64-apple-darwin/release/loch.
// Writes <out-dir>/<package>/ for the wrapper and each platform given; the
// wrapper's optionalDependencies always name every platform, pinned to this
// exact version, so npm installs the one matching the host and only that.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const platforms = JSON.parse(fs.readFileSync(path.join(here, "platforms.json"), "utf8"));
const [version, out, ...binaries] = process.argv.slice(2);
if (!version || !out) {
  console.error("usage: assemble.mjs <version> <out-dir> <target>=<binary> ...");
  process.exit(2);
}

const wrapper = JSON.parse(fs.readFileSync(path.join(here, "loch", "package.json"), "utf8"));
const wrapperDir = path.join(out, wrapper.name);
fs.cpSync(path.join(here, "loch"), wrapperDir, { recursive: true });
fs.copyFileSync(path.join(here, "platforms.json"), path.join(wrapperDir, "platforms.json"));
wrapper.version = version;
wrapper.optionalDependencies = Object.fromEntries(
  Object.values(platforms).map((p) => [p.package, version]),
);
fs.writeFileSync(path.join(wrapperDir, "package.json"), JSON.stringify(wrapper, null, 2) + "\n");

for (const spec of binaries) {
  const [target, binary] = spec.split("=");
  const platform = platforms[target];
  if (!platform) throw new Error(`no npm platform for ${target}`);
  const dir = path.join(out, platform.package);
  fs.mkdirSync(dir, { recursive: true });
  fs.copyFileSync(binary, path.join(dir, platform.bin));
  fs.chmodSync(path.join(dir, platform.bin), 0o755);
  const manifest = {
    name: platform.package,
    version,
    description: `The ${platform.os}-${platform.cpu} binary for @ness-dev/loch`,
    license: "MIT",
    repository: wrapper.repository,
    os: [platform.os],
    cpu: [platform.cpu],
    files: [platform.bin],
  };
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
}
