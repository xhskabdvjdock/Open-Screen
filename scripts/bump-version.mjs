// Bumps the app version in every place Tauri/npm need, keeping them in sync:
//   package.json <-> src-tauri/tauri.conf.json <-> src-tauri/Cargo.toml
//   (+ About header in src/settings.html, Cargo.lock entry)
// Usage: node scripts/bump-version.mjs patch|minor|major
import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const kind = process.argv[2];

if (!["patch", "minor", "major"].includes(kind)) {
  console.error("Usage: node scripts/bump-version.mjs patch|minor|major");
  process.exit(1);
}

const bump = (v) => {
  const m = v.trim().match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (!m) throw new Error(`Bad semver: ${v}`);
  let [major, minor, patch] = [+m[1], +m[2], +m[3]];
  if (kind === "major") return `${major + 1}.0.0`;
  if (kind === "minor") return `${major}.${minor + 1}.0`;
  return `${major}.${minor}.${patch + 1}`;
};

const pkgPath = join(root, "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));
const next = bump(pkg.version);
pkg.version = next;
writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");

// tauri.conf.json (Tauri reads this for the bundle version)
const confPath = join(root, "src-tauri", "tauri.conf.json");
const conf = JSON.parse(readFileSync(confPath, "utf8"));
conf.version = next;
writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");

// Cargo.toml — first `version = "x"` under [package] only
const cargoPath = join(root, "src-tauri", "Cargo.toml");
let cargo = readFileSync(cargoPath, "utf8");
let done = false;
cargo = cargo.replace(/^(version\s*=\s*")\d+\.\d+\.\d+(")/m, (...a) => {
  if (done) return a[0];
  done = true;
  return `${a[1]}${next}${a[2]}`;
});
writeFileSync(cargoPath, cargo);

// About header in Settings (cosmetic, keeps UI honest)
const aboutPath = join(root, "src", "settings.html");
try {
  const html = readFileSync(aboutPath, "utf8").replace(
    /(<h3>Open Screen )\d+\.\d+\.\d+(<\/h3>)/,
    `$1${next}$2`,
  );
  writeFileSync(aboutPath, html);
} catch { /* non-fatal */ }

// Cargo.lock entry for `open-screen` (cargo refreshes it anyway on build)
const lockPath = join(root, "src-tauri", "Cargo.lock");
try {
  const lock = readFileSync(lockPath, "utf8").replace(
    /(name\s*=\s*"open-screen"\nversion\s*=\s*")\d+\.\d+\.\d+(")/,
    `$1${next}$2`,
  );
  writeFileSync(lockPath, lock);
} catch { /* non-fatal (lock may be git-ignored) */ }

console.log(`version:${kind} -> ${next}`);
