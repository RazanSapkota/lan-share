// Writes one version number into every file that carries it.
//
//   node tools/set-version.mjs 0.4.0
//   npm run version:set -- 0.4.0
//
// Run by hand before cutting a release. Like make-icons.mjs, this is never run
// by the build and depends on nothing outside the Node standard library.
//
// Four files, and only two of them are load-bearing:
//
//   Cargo.toml         `env!("CARGO_PKG_VERSION")` in models.rs, which is what
//                      the app reports to peers and to browsers.
//   tauri.conf.json    the Windows version resource. tauri-build reads this
//                      field and only this field -- it has no Cargo.toml
//                      fallback -- so dropping it would leave the exe with a
//                      blank File version in Properties -> Details.
//   Cargo.lock         the crate's own entry. Cargo rewrites this on the next
//                      command anyway; doing it here keeps the bump to one
//                      commit instead of leaving a stray diff behind.
//   package.json       read by nothing: `private: true`, no publish step, and
//                      `beforeBuildCommand` is empty. Kept in step because a
//                      version that is merely decorative still gets read by
//                      people.
//
// The release workflow refuses to build when the tag disagrees with the first
// two, which is what this exists to prevent.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");

/// Major.minor.patch, no prerelease or build metadata. Narrower than semver on
/// purpose: `to_winres_version` truncates anything past the third field, and a
/// tag that survives here but renders differently in the installer is the exact
/// mismatch the workflow's guard is for.
const VERSION_RE = /^\d+\.\d+\.\d+$/;

/// One edit per file, each anchored tightly enough that it cannot match a
/// dependency's version. A pattern that stops matching is a hard error rather
/// than a silent no-op: an unbumped manifest does fail CI at the tag check, but
/// only after someone has already drafted the release.
///
/// `core.autocrlf` is true and there is no `.gitattributes`, so a fresh clone
/// checks all four out with CRLF endings. Every trailing `\r` is captured, not
/// skipped, so a rewritten line keeps the ending its neighbours have.
const TARGETS = [
  {
    file: "package.json",
    // The first `"version"` at the top level. Two spaces of indent is the file's
    // own formatting, and no nested object in it sits at that depth. Both JSON
    // files carry a trailing comma on this line; neither is the last key.
    find: /^(  "version": ")[^"]+(",?\r?)$/m,
  },
  {
    file: "src-tauri/tauri.conf.json",
    find: /^(  "version": ")[^"]+(",?\r?)$/m,
  },
  {
    file: "src-tauri/Cargo.toml",
    // Anchored to the file head: `[package]` is the first table, so the first
    // bare `version =` belongs to this crate and not to a dependency.
    find: /^(version = ")[^"]+("\r?)$/m,
  },
  {
    file: "src-tauri/Cargo.lock",
    // The lockfile lists 400-odd `version = ` lines. Only the one directly
    // under this crate's name is ours.
    find: /^(name = "lan_share_tauri"\r?\nversion = ")[^"]+("\r?)$/m,
  },
];

const version = process.argv[2];

if (!version) {
  console.error("usage: node tools/set-version.mjs <major.minor.patch>");
  process.exit(2);
}

if (!VERSION_RE.test(version)) {
  console.error(`not a major.minor.patch version: ${version}`);
  process.exit(2);
}

let changed = 0;

for (const { file, find } of TARGETS) {
  const path = join(ROOT, file);
  const before = readFileSync(path, "utf8");
  const match = before.match(find);

  if (!match) {
    console.error(`no version line found in ${file} -- has its format changed?`);
    process.exit(1);
  }

  const was = match[0].match(/"([^"]+)",?\r?$/)?.[1] ?? match[0];

  if (was === version) {
    console.log(`  ${file} already ${version}`);
    continue;
  }

  // Written back byte-for-byte apart from the number: no JSON round-trip, which
  // would reformat the file and reorder nothing usefully, and no TOML parser at
  // all.
  writeFileSync(path, before.replace(find, `$1${version}$2`));
  console.log(`  ${file} ${was} -> ${version}`);
  changed++;
}

console.log(
  changed === 0
    ? `\nAlready at ${version}; nothing to do.`
    : `\nNow at ${version}. Commit, push, then draft the release with tag v${version}.`,
);
