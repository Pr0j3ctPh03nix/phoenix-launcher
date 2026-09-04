// Release the launcher:  node dev/release.js <command>
//
//   note <section> <text>     append a bullet to the staging changelog (CHANGELOG.md)
//   show                      print what the next release would ship
//   cut <version|patch|minor|major> [--yes] [--no-push]
//
// `cut` without --yes is a REHEARSAL: it runs every check and prints the exact bump and tag body,
// and writes nothing.
//
// `cut` performs the whole ritual CHANGELOG.md's header used to describe by hand: check, bump the
// FOUR version sites, commit, write the annotated tag whose BODY is the changelog, verify that tag
// object, push, and reset the staging file. Every step that is silent when it goes wrong is
// checked here, because the two that matter have no symptom until clients see them:
//
//   * a tag whose version files disagree makes every client offer an update that never clears
//     (self-update compares the tag against the binary's CARGO_PKG_VERSION), and
//   * `git tag` strips `#`-leading lines by default, which deletes every `####` heading from the
//     body the app renders.
//
// Nothing here is a substitute for release.yml's own checks — it is the same questions asked
// before the tag is pushed rather than after, when the answer costs another release.
"use strict";
const fs = require("fs");
const path = require("path");
const vm = require("vm");
const { execFileSync } = require("child_process");

const ROOT = path.join(__dirname, "..");
const CHANGELOG = path.join(ROOT, "CHANGELOG.md");
const MAIN_JS = path.join(ROOT, "frontend", "main.js");
const SEP = "---"; // everything after the first one of these is the release body

// The four places the version lives. Cargo.lock is one of them: CI builds `--locked`, so a lock
// still naming the old version fails the release AFTER the version check has passed and the
// multi-minute build has started.
const SITES = {
  "src-tauri/Cargo.toml": {
    read: (s) => s.match(/^version = "(.+)"$/m)?.[1],
    write: (s, v) => s.replace(/^version = ".+"$/m, `version = "${v}"`),
  },
  "src-tauri/tauri.conf.json": {
    read: (s) => JSON.parse(s).version,
    // rewritten as TEXT, not JSON.stringify: reserializing reformats a file nobody asked to
    // reformat, and the diff of a release commit should be four version lines
    write: (s, v) => s.replace(/("version"\s*:\s*")[^"]+(")/, `$1${v}$2`),
  },
  "package.json": {
    read: (s) => JSON.parse(s).version,
    write: (s, v) => s.replace(/("version"\s*:\s*")[^"]+(")/, `$1${v}$2`),
  },
  "src-tauri/Cargo.lock": {
    read: (s) => s.match(/\[\[package\]\]\nname = "phoenix-launcher"\nversion = "(.+)"/)?.[1],
    write: (s, v) =>
      s.replace(/(\[\[package\]\]\nname = "phoenix-launcher"\nversion = ")[^"]+(")/, `$1${v}$2`),
  },
};

// ---- plumbing ----

const die = (msg) => {
  console.error(`release: ${msg}`);
  process.exit(1);
};
const git = (...args) =>
  execFileSync("git", args, { cwd: ROOT, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
const read = (rel) => fs.readFileSync(path.join(ROOT, rel), "utf8");

// The sections the APP renders, read out of main.js rather than copied here: a heading it does not
// recognize still ships, it just sinks below the known trio with no icon and no colour — a defect
// with no symptom until someone opens What's new. Two literals, both ending at a `};` in column 0.
function frontendSections() {
  const src = fs.readFileSync(MAIN_JS, "utf8");
  const lift = (name) => {
    const m = src.match(new RegExp(`const ${name} = \\{[\\s\\S]*?\\n\\};`));
    if (!m) die(`could not find ${name} in frontend/main.js — has it been renamed?`);
    return m[0];
  };
  const ctx = {};
  vm.runInNewContext(
    `${lift("NOTE_SECTIONS")}\n${lift("NOTE_SECTION_ALIASES")}\n__out = { NOTE_SECTIONS, NOTE_SECTION_ALIASES };`,
    ctx
  );
  const { NOTE_SECTIONS, NOTE_SECTION_ALIASES } = ctx.__out;
  if (!NOTE_SECTION_ALIASES.added) die("the frontend's section table came out empty");
  return { sections: NOTE_SECTIONS, aliases: NOTE_SECTION_ALIASES };
}

// ---- the staging changelog ----
//
// Split at the first `---`: everything above is the local how-to header (never shipped), everything
// below is the release body, verbatim. That split is the whole file format.
function loadChangelog() {
  if (!fs.existsSync(CHANGELOG)) die(`${CHANGELOG} is missing — it is gitignored, so a fresh clone has none`);
  const raw = fs.readFileSync(CHANGELOG, "utf8");
  const lines = raw.split(/\r?\n/);
  const at = lines.findIndex((l) => l.trim() === SEP);
  if (at < 0) die(`CHANGELOG.md has no '${SEP}' line — the header and the release body are split on it`);
  return { raw, head: lines.slice(0, at + 1).join("\n"), body: lines.slice(at + 1).join("\n").trim() };
}

// What the tag body would be, plus every reason it is not shippable. Headings are checked against
// the app's own alias table; a section with no bullets is checked because it renders as a heading
// over nothing.
function inspectBody(body, aliases) {
  const problems = [];
  const seen = [];
  let cur = null;
  for (const raw of body.split(/\r?\n/)) {
    const line = raw.trim();
    const h = line.match(/^#{1,6}\s+(.*)$/);
    if (h) {
      const name = h[1].trim();
      cur = { name, bullets: 0, known: !!aliases[name.toLowerCase()] };
      seen.push(cur);
      if (!cur.known) problems.push(`"${name}" is not a section the app renders (${Object.keys(aliases).join(" | ")})`);
    } else if (/^[-*]\s+/.test(line) && cur) cur.bullets++;
  }
  if (!body) problems.push("nothing to release — the staging changelog holds no notes");
  else if (!seen.length) problems.push("no #### section headings — the app groups notes by them");
  for (const s of seen) if (!s.bullets) problems.push(`section "${s.name}" has no bullets under it`);
  return { sections: seen, problems };
}

// ---- versions ----

const parseVer = (v) => {
  const m = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(String(v).trim());
  return m ? m.slice(1, 4).map(Number) : null;
};
const cmpVer = (a, b) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2];

function latestTag() {
  const tags = git("tag", "--list", "v*", "--sort=-v:refname").split("\n").filter(Boolean);
  return tags[0] || null;
}

function resolveVersion(arg, from) {
  const bump = { major: 0, minor: 1, patch: 2 }[arg];
  if (bump == null) {
    const v = parseVer(arg);
    if (!v) die(`"${arg}" is neither a version (X.Y.Z) nor major|minor|patch`);
    return v;
  }
  if (!from) die(`cannot ${arg}-bump: no vX.Y.Z tag exists to count from`);
  const v = [...from];
  v[bump]++;
  for (let i = bump + 1; i < 3; i++) v[i] = 0;
  return v;
}

// ---- commands ----

function cmdNote(argv) {
  const { sections, aliases } = frontendSections();
  const [rawSection, ...rest] = argv;
  const text = rest.join(" ").trim();
  if (!rawSection || !text) die('usage: node dev/release.js note <section> "<text>"');
  const canonical = aliases[rawSection.trim().toLowerCase()];
  if (!canonical)
    die(`"${rawSection}" is not a section the app renders — use one of: ${Object.keys(aliases).join(", ")}`);

  const { head, body } = loadChangelog();
  const title = canonical[0].toUpperCase() + canonical.slice(1);
  const heading = `#### ${title}`;
  const lines = body ? body.split(/\r?\n/) : [];
  const isHeading = (l) => /^#{1,6}\s+/.test(l.trim());
  // the section this bullet belongs to, matched through the alias table so an existing "#### New"
  // receives an `added` note instead of growing a second heading beside it
  let at = lines.findIndex(
    (l) => isHeading(l) && aliases[l.trim().replace(/^#+\s+/, "").toLowerCase()] === canonical
  );
  if (at < 0) {
    // new section, placed in the app's own rank order so the file reads the way the page will
    const rank = (name) => sections[aliases[name.trim().replace(/^#+\s+/, "").toLowerCase()]]?.rank ?? 3;
    const mine = sections[canonical].rank;
    let insert = lines.length;
    for (let i = 0; i < lines.length; i++) {
      if (isHeading(lines[i]) && rank(lines[i]) > mine) { insert = i; break; }
    }
    const block = [heading, "", `- ${text}`, ""];
    lines.splice(insert, 0, ...(insert === lines.length && lines.length ? ["", ...block] : block));
  } else {
    // append after the section's LAST bullet, so notes read in the order they were written
    let end = at + 1;
    for (let i = at + 1; i < lines.length && !isHeading(lines[i]); i++) if (lines[i].trim()) end = i;
    lines.splice(end + 1, 0, `- ${text}`);
  }
  fs.writeFileSync(CHANGELOG, `${head}\n\n${lines.join("\n").trim()}\n`, "utf8");
  console.log(`noted under ${heading}: ${text}`);
}

function cmdShow() {
  const { aliases } = frontendSections();
  const { body } = loadChangelog();
  const { sections, problems } = inspectBody(body, aliases);
  const next = latestTag();
  console.log(`staging changelog — ${sections.length} section(s), since ${next || "the beginning"}\n`);
  console.log(body || "(empty)");
  if (problems.length) {
    console.log("");
    for (const p of problems) console.log(`  not shippable: ${p}`);
  }
}

function cmdCut(argv) {
  const noPush = argv.includes("--no-push");
  const yes = argv.includes("--yes");
  const target = argv.find((a) => !a.startsWith("-"));
  if (!target) die("usage: node dev/release.js cut <version|patch|minor|major> [--yes] [--no-push]");

  const { aliases } = frontendSections();
  const prev = latestTag();
  const version = resolveVersion(target, prev && parseVer(prev)).join(".");
  const tag = `v${version}`;

  // ---- checks, cheapest first, all of them before anything is written ----
  const fail = [];
  const branch = git("rev-parse", "--abbrev-ref", "HEAD");
  if (branch !== "main") fail.push(`on branch ${branch}, not main`);
  const dirty = git("status", "--porcelain");
  if (dirty) fail.push(`working tree is not clean:\n${dirty.split("\n").map((l) => `    ${l.trim()}`).join("\n")}`);

  // BEHIND origin is refused (the release would ship a stale tree); ahead is the normal case, since
  // the work being released is usually unpushed.
  try {
    git("fetch", "--quiet", "origin", "main", "--tags");
    const behind = git("rev-list", "--count", "HEAD..origin/main");
    if (behind !== "0") fail.push(`${behind} commit(s) behind origin/main — pull first`);
  } catch (e) {
    fail.push(`could not reach origin: ${String(e.stderr || e.message).trim()}`);
  }

  if (prev && cmpVer(parseVer(version), parseVer(prev)) <= 0)
    fail.push(`${tag} is not newer than the latest tag ${prev}`);
  if (git("tag", "--list", tag)) fail.push(`${tag} already exists locally`);
  try {
    if (git("ls-remote", "--tags", "origin", `refs/tags/${tag}`)) fail.push(`${tag} already exists on origin`);
  } catch { /* the fetch failure above already reported this */ }

  const { head, body } = loadChangelog();
  const { sections, problems } = inspectBody(body, aliases);
  fail.push(...problems);

  if (fail.length) {
    for (const f of fail) console.error(`  refused: ${f}`);
    process.exit(1);
  }

  // ---- what it would do ----
  const subject = `Phoenix Launcher ${tag}`;
  const current = Object.fromEntries(Object.entries(SITES).map(([f, s]) => [f, s.read(read(f))]));
  console.log(`${prev || "(none)"} -> ${tag}\n`);
  for (const [f, v] of Object.entries(current)) console.log(`  ${f}: ${v} -> ${version}`);
  console.log(`\n  tag subject: ${subject}`);
  console.log(`  tag body:    ${sections.map((s) => `${s.name} (${s.bullets})`).join(", ")}\n`);
  console.log(body.replace(/^/gm, "  | "));
  console.log("");

  // Everything above this line only READ. A release is irreversible in the direction that matters —
  // a published tag cannot be unpublished from clients that have already seen it — so the default
  // is the rehearsal and the commitment is a flag, rather than a prompt nobody can answer from a
  // script and everybody learns to answer without reading.
  if (!yes) {
    console.log("rehearsal only — nothing written. Pass --yes to bump, tag and push.");
    return;
  }

  // ---- bump ----
  for (const [f, site] of Object.entries(SITES)) {
    const p = path.join(ROOT, f);
    const next = site.write(fs.readFileSync(p, "utf8"), version);
    fs.writeFileSync(p, next, "utf8");
    const got = site.read(next);
    if (got !== version) die(`${f} still reads ${got} after the bump — its version line did not match`);
  }
  // The lock is bumped by hand above, so let cargo be the judge of whether it is coherent: this is
  // the same assertion CI's `--locked` build makes, minus the multi-minute build in front of it.
  try {
    execFileSync("cargo", ["metadata", "--locked", "--offline", "--format-version", "1"], {
      cwd: path.join(ROOT, "src-tauri"),
      stdio: ["ignore", "ignore", "pipe"],
    });
  } catch (e) {
    die(`Cargo.lock does not match Cargo.toml after the bump:\n${String(e.stderr || "").trim()}`);
  }

  // The commit names the four files EXPLICITLY rather than `-am`: a release commit is four version
  // lines, and anything that appeared in the tree since the clean-tree check above is not part of
  // the release just because it happened to be there.
  const was = git("rev-parse", "HEAD"); // rolled back to by SHA, never by HEAD~1
  git("add", ...Object.keys(SITES));
  git("commit", "-m", `chore: ${version}`);

  // --cleanup=verbatim: git's default strips every `#`-leading line as a comment, which silently
  // deletes the `####` headings the app groups notes by. Nothing warns — so the tag object is read
  // back and compared here, and a mismatch unwinds the release rather than shipping a body the
  // What's-new page cannot group.
  git("tag", "-a", tag, "--cleanup=verbatim", "-m", `${subject}\n\n${body}\n`);
  const written = git("tag", "-l", "--format=%(contents:body)", tag).trim();
  if (written !== body.trim()) {
    git("tag", "-d", tag);
    git("reset", "--hard", was);
    die("the tag body git stored is not the body it was given (headings stripped?) — tag and commit rolled back");
  }
  console.log(`tagged ${tag}, body verified (${written.split("\n").length} lines)`);

  if (noPush) {
    console.log(`--no-push: push it yourself with\n  git push origin main && git push origin ${tag}`);
  } else {
    // main FIRST: release.yml triggers on the tag, and it should never build a commit that is not
    // yet on the branch it was released from.
    git("push", "origin", "main");
    git("push", "origin", tag);
    console.log(`pushed main and ${tag} — release.yml builds, seals, then un-drafts the release`);
  }

  // The notes now live in the tag object, which is what the app reads and what CI copies into the
  // release body. Resetting here rather than after the push keeps the two from ever disagreeing:
  // once the tag exists, the file is a duplicate.
  const nextHead = head.replace(/^# next \(unreleased\).*$/m, `# next (unreleased) — since ${tag}`);
  fs.writeFileSync(CHANGELOG, `${nextHead}\n`, "utf8");
  console.log(`CHANGELOG.md reset — previous notes: git tag -l --format='%(contents:body)' ${tag}`);
}

const [cmd, ...rest] = process.argv.slice(2);
if (cmd === "note") cmdNote(rest);
else if (cmd === "show") cmdShow();
else if (cmd === "cut") cmdCut(rest);
else {
  console.log("usage: node dev/release.js note <section> \"<text>\" | show | cut <version|patch|minor|major>");
  process.exit(cmd ? 1 : 0);
}
