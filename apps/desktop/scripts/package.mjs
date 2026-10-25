// Build the desktop app with the `twinbase` command-line tool inside it, so one installer gives
// both (agents reach TwinBase through `twinbase mcp`). Usage:
//   npm run package -- [--target <triple>] [other `tauri build` arguments…]
//
// Tauri takes the CLI as an "external binary" (src-tauri/tauri.bundle.conf.json): it must be at
// src-tauri/binaries/twinbase-<target triple> before `tauri build`. It lands next to the app's
// executable (Contents/MacOS on macOS, the install folder on Windows, /usr/bin for .deb/.rpm).
// Only packaging uses that config, so `cargo build` and `tauri dev` don't need the file.
//
// No shell anywhere: on Windows a shell would mangle arguments.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const app = path.resolve(import.meta.dirname, "..");
const root = path.resolve(app, "..", "..");
const args = process.argv.slice(2);

/** Remove `--name value` from the arguments; returns the value. */
function take(name) {
  const i = args.indexOf(name);
  if (i < 0) return null;
  const [, value] = args.splice(i, 2);
  return value;
}

const target = take("--target");
const host = /host: (\S+)/.exec(execFileSync("rustc", ["-vV"]).toString())[1];
const triple = target ?? host;
const exe = triple.includes("windows") ? ".exe" : "";

function run(cmd, cmdArgs, cwd = root) {
  console.log(`> ${cmd} ${cmdArgs.join(" ")}`);
  execFileSync(cmd, cmdArgs, { stdio: "inherit", cwd });
}

/** Build the CLI for one real target; returns the binary's path. */
function buildCli(t) {
  const cross = t !== host;
  run("cargo", ["build", "--release", "-p", "twinbase-cli", ...(cross ? ["--target", t] : [])]);
  return path.join(root, "target", ...(cross ? [t] : []), "release", `twinbase${t.includes("windows") ? ".exe" : ""}`);
}

const dir = path.join(app, "src-tauri", "binaries");
fs.mkdirSync(dir, { recursive: true });
const dest = path.join(dir, `twinbase-${triple}${exe}`);
if (triple === "universal-apple-darwin") {
  // One binary for Apple silicon and Intel Macs, like the app. Tauri compiles the app once per
  // architecture and each pass looks for its own copy too.
  const arches = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
  const parts = arches.map(buildCli);
  arches.forEach((arch, i) => fs.copyFileSync(parts[i], path.join(dir, `twinbase-${arch}`)));
  run("lipo", ["-create", "-output", dest, ...parts]);
} else {
  fs.copyFileSync(buildCli(triple), dest);
}
fs.chmodSync(dest, 0o755);

const tauri = path.join(app, "node_modules", "@tauri-apps", "cli", "tauri.js");
const config = ["--config", path.join("src-tauri", "tauri.bundle.conf.json")];
run(process.execPath, [tauri, "build", ...config, ...(target ? ["--target", target] : []), ...args], app);
