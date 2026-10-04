// Build one .vsix per platform with the grebe binary inside, plus a universal
// .vsix without one, from the release archives.
//
//   node package-targets.js <dir with grebe-<rust-target>.tar.gz|.zip> <out dir>
//
// The Marketplace hands each editor the package for its platform, so
// installing the extension is enough: no separate grebe install, and the
// server always matches the extension's version. Editors on a platform with
// no package of its own (Windows on ARM, say) get the universal one, which
// looks for grebe in grebe.path, ~/.local/bin and PATH.
const { execFileSync } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

// Rust target -> VS Code platform targets. The musl build is static, so the
// same binary serves glibc Linux and Alpine.
const TARGETS = {
  "aarch64-apple-darwin": ["darwin-arm64"],
  "x86_64-apple-darwin": ["darwin-x64"],
  "x86_64-unknown-linux-musl": ["linux-x64", "alpine-x64"],
  "aarch64-unknown-linux-gnu": ["linux-arm64"],
  "x86_64-pc-windows-msvc": ["win32-x64"],
};

const here = __dirname;
const [archives, out] = process.argv.slice(2).map((p) => path.resolve(p));
if (!archives || !out) {
  console.error("usage: node package-targets.js <archives dir> <out dir>");
  process.exit(2);
}
const version = JSON.parse(fs.readFileSync(path.join(here, "package.json"), "utf8")).version;
const bin = path.join(here, "bin");
fs.mkdirSync(out, { recursive: true });

function vsce(args) {
  execFileSync("npx", ["--yes", "@vscode/vsce", "package", ...args], { cwd: here, stdio: "inherit" });
}

function extract(archive, rustTarget) {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-pkg-"));
  if (archive.endsWith(".zip")) execFileSync("unzip", ["-q", archive, "-d", scratch]);
  else execFileSync("tar", ["xzf", archive, "-C", scratch]);
  const exe = rustTarget.includes("windows") ? "grebe.exe" : "grebe";
  const file = path.join(scratch, `grebe-${rustTarget}`, exe);
  if (!fs.existsSync(file)) throw new Error(`${archive} has no grebe-${rustTarget}/${exe}`);
  return { file, exe };
}

let built = 0;
try {
  for (const [rustTarget, platforms] of Object.entries(TARGETS)) {
    const archive = ["tar.gz", "zip"]
      .map((ext) => path.join(archives, `grebe-${rustTarget}.${ext}`))
      .find((p) => fs.existsSync(p));
    if (!archive) throw new Error(`no release archive for ${rustTarget} in ${archives}`);
    const { file, exe } = extract(archive, rustTarget);
    fs.rmSync(bin, { recursive: true, force: true });
    fs.mkdirSync(bin);
    fs.copyFileSync(file, path.join(bin, exe));
    fs.chmodSync(path.join(bin, exe), 0o755);
    for (const platform of platforms) {
      vsce(["--target", platform, "-o", path.join(out, `grebe-${platform}-${version}.vsix`)]);
      built++;
    }
  }
} finally {
  fs.rmSync(bin, { recursive: true, force: true });
}
vsce(["-o", path.join(out, `grebe-${version}.vsix`)]);
console.log(`built ${built} platform .vsix files and the universal one in ${out}`);
