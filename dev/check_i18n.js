// Frontend consistency gate:  node dev/check_i18n.js   (or `bun dev/check_i18n.js`)
//
//   - every key the DOM/JS asks for exists in the EN table
//   - EN and RU hold the same key set (a missing RU key silently falls back to English, which
//     looks like a translation nobody wrote rather than a bug)
//   - every $("id") main.js reaches for exists in index.html
//
// Exits non-zero on any of the three, so it can gate a commit.
const fs = require("fs");
const vm = require("vm");
const path = require("path");

const FE = path.join(__dirname, "..", "frontend");
const html = fs.readFileSync(path.join(FE, "index.html"), "utf8");
const js = fs.readFileSync(path.join(FE, "main.js"), "utf8");
const i18nSrc = fs.readFileSync(path.join(FE, "i18n.js"), "utf8");

// i18n.js only touches `document` inside functions we never call, so a stub context is enough
const ctx = { document: { documentElement: {}, querySelectorAll: () => [] }, navigator: { language: "en" } };
vm.createContext(ctx);
vm.runInContext(i18nSrc + "\n;globalThis.__T = I18N;", ctx);
const I18N = ctx.__T;

const en = new Set(Object.keys(I18N.en));
const ru = new Set(Object.keys(I18N.ru));
const used = new Set();
for (const m of html.matchAll(/data-i18n(?:-ph|-title)?="([^"]+)"/g)) used.add(m[1]);
for (const m of js.matchAll(/\bt\(\s*"([^"]+)"/g)) used.add(m[1]);
// families built at runtime from backend data (launch flags, file states) — not literals
const dynamic = (k) => k.startsWith("set.flag.") || k.startsWith("fstate.");

const ids = new Set([...html.matchAll(/id="([^"]+)"/g)].map((m) => m[1]));
const wanted = new Set([...js.matchAll(/\$\("([^"]+)"\)/g)].map((m) => m[1]));

const problems = [
  ["keys used but missing from en", [...used].filter((k) => !en.has(k) && !dynamic(k))],
  ["keys in en but missing from ru", [...en].filter((k) => !ru.has(k))],
  ["keys in ru but missing from en", [...ru].filter((k) => !en.has(k))],
  ["ids used by main.js but absent from index.html", [...wanted].filter((i) => !ids.has(i))],
].filter(([, list]) => list.length);

console.log(`referenced=${used.size} en=${en.size} ru=${ru.size} ids=${ids.size}`);
for (const [what, list] of problems) console.error(`FAIL ${what}: ${list.join(", ")}`);
if (problems.length) process.exit(1);
console.log("ok");
// Keys with no literal reference are NOT an error: several are looked up indirectly through the
// ERR_WORDS / ERR_HINTS / primary-label maps in main.js.
