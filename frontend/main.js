const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

// Internal knob: the Advanced settings block (source repo / access token). Off = not rendered at
// all; the baked-in defaults apply. Flip to true for maintainer builds.
const SHOW_ADVANCED = false;

const state = {
  busy: false,
  lastCheck: null,     // last CheckView
  primaryMode: "check", // "check" | "apply" | "manage" | "play" | "updateLauncher"
  launcherUpdate: null, // LauncherUpdateView while a newer launcher is pending, else null.
                        // Only ever set from a SUCCESSFUL launcher_check — a failed one leaves it
                        // untouched, so "couldn't ask" never reads as "nothing to install".
  launcherVersion: "",  // this build's version, from launcher_info
  justUpdated: false,   // this process was started by a self-update; shown once, then cleared
  hasToken: false,
  renderer: "dx11",
  launchFlags: [],    // [{id, args, enabled}] as loaded from settings, toggled in place
  afTarget: null,      // "setup" | "settings" — where an autofind pick lands
  afUnlisten: null,
  afBusy: false,       // a scan invoke is in flight (double-start guard)
  aeDirty: false,
  aeReadOnly: false,   // autoexec shown read-only (non-UTF-8 file or failed read)
  gameRunning: false,  // polled — the game is currently running
  busyOwner: null,     // token of the flow holding the busy lock (see acquireBusy)
  gameDamaged: 0,      // files a verify reported damaged and the user declined to repair
  fileEls: new Map(),  // dest -> every <li> describing it in the managed-files list (live dl bars)
  filesOpen: new Set(), // which managed-files categories are expanded — remembered for the session
  filesShown: null,     // the payload those rows were built from, so a toggle re-renders it
  applyWrites: new Set(), // dests the RUNNING apply will write beyond the release's own changes
                          // (a restore's picks) — what a category row waits for before settling
  settingsSnap: null,  // settings-form snapshot at open, for the discard-changes guard
  settingsLoaded: null, // {repo, game} as loaded — a save that changes them re-checks
  settingsTab: "general", // remembered for the session: a repeat visit lands where the user was
  wnTab: "phoenix",    // which What's-new history was last read (same session-memory rule)
  tokenClear: false,   // "Clear" was pressed: the saved token is removed on save
};

// ---- markdown-lite: the notes are trusted (our own manifest) but escape anyway, then apply the
// changelog subset: headings, bullet + ordered lists, ``` fences, **bold**, *italic*, `code`,
// [links](https://вЂ¦). No raw HTML from the source ever reaches innerHTML; links go through the
// open_url command (http/https only). ----
function escHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
// Known changelog section headings get an icon + a per-section accent (see .notes h4.sec-* in
// style.css). Matched on the raw heading text; anything else stays a plain h4.
const NOTE_SECTIONS = {
  added:   { cls: "sec-added",   rank: 0, icon: '<path d="M6 2.25v7.5M2.25 6h7.5"/>' },
  fixed:   { cls: "sec-fixed",   rank: 1, icon: '<path d="M2 6.5l2.5 2.75L10 2.75"/>' },
  changed: { cls: "sec-changed", rank: 2, icon: '<path d="M1.75 4h6.75L6.25 1.75M10.25 8H3.5L5.75 10.25"/>' },
};
const NOTE_SECTION_ALIASES = {
  new: "added", changed: "changed", improved: "changed", improvements: "changed",
  added: "added", fixed: "fixed", fixes: "fixed", bugfixes: "fixed", "bug fixes": "fixed",
};
function renderNotes(md) {
  const inline = (s) => {
    s = escHtml(s);
    // protect code spans from the other inline rules
    const codes = [];
    s = s.replace(/`([^`]+)`/g, (_, c) => { codes.push(c); return `\x00${codes.length - 1}\x00`; });
    s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, txt, url) =>
      /^https?:\/\//i.test(url) ? `<a href="#" data-url="${url}">${txt}</a>` : txt);
    s = s.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
    // single-* italics only вЂ” _underscores_ stay literal (file_names are common in changelogs)
    s = s.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
    return s.replace(/\x00(\d+)\x00/g, (_, i) => `<code>${codes[i]}</code>`);
  };
  // Output accumulates in SEGMENTS, one per section, so recognized sections can be reordered to
  // the canonical Added → Fixed → Changed regardless of how the release author wrote them.
  // Preamble (before any heading) stays first (-1); unknown headings sink after the known trio
  // (rank 3) but keep their relative order — the sort is stable.
  const segs = [{ rank: -1, html: "" }];
  let cur = segs[0];
  let items = null;    // open list's item texts
  let listTag = "ul";  // tag of the open list
  let para = null;     // open paragraph text
  let fence = null;    // open ``` fence's raw lines
  const flushList = () => { if (items) { cur.html += `<${listTag}>` + items.map((t) => `<li>${inline(t)}</li>`).join("") + `</${listTag}>`; items = null; } };
  const flushPara = () => { if (para != null) { cur.html += `<p>${inline(para)}</p>`; para = null; } };
  const flushAll = () => { flushList(); flushPara(); };
  const openList = (tag) => { if (!items || listTag !== tag) { flushList(); items = []; listTag = tag; } };
  for (const raw of md.split(/\r?\n/)) {
    if (fence !== null) {
      if (/^```/.test(raw.trim())) { cur.html += `<pre><code>${escHtml(fence.join("\n"))}</code></pre>`; fence = null; }
      else fence.push(raw);
      continue;
    }
    const line = raw.trim();
    if (/^```/.test(line)) { flushAll(); fence = []; continue; }
    if (!line) { flushAll(); continue; }
    let m;
    if ((m = line.match(/^#{1,6}\s+(.*)$/))) {
      flushAll();
      const sec = NOTE_SECTIONS[NOTE_SECTION_ALIASES[m[1].trim().toLowerCase()]];
      cur = { rank: sec ? sec.rank : 3, html: "" };
      segs.push(cur);
      cur.html += sec
        ? `<h4 class="sec ${sec.cls}"><svg class="sec-ic" viewBox="0 0 12 12" aria-hidden="true">${sec.icon}</svg>${inline(m[1])}</h4>`
        : `<h4>${inline(m[1])}</h4>`;
    }
    else if ((m = line.match(/^[-*]\s+(.*)$/))) { flushPara(); openList("ul"); items.push(m[1]); }
    else if ((m = line.match(/^\d+[.)]\s+(.*)$/))) { flushPara(); openList("ol"); items.push(m[1]); }
    // lazy continuation of the current bullet / paragraph (wrapped line)
    else if (items) items[items.length - 1] += " " + line;
    else para = para == null ? line : para + " " + line;
  }
  if (fence !== null) cur.html += `<pre><code>${escHtml(fence.join("\n"))}</code></pre>`; // unclosed fence
  flushAll();
  return segs.sort((a, b) => a.rank - b.rank).map((s) => s.html).join(""); // stable — ties keep author order
}

// ---- modals ----
// The visible modal, if any. Modals live OUTSIDE .stage, so the view underneath is made `inert`
// while one is open: without it Tab walked into the view behind the dialog and Space activated
// whatever it landed on — including Play, from behind a "repair these files?" confirm.
function openModal() {
  return document.querySelector(".modal:not(.hidden)");
}
function syncModalLayer() {
  const open = !!openModal();
  document.querySelector(".stage").inert = open;
  if (!open) return;
  // focus something inside the dialog if the click that opened it left focus behind
  const card = openModal().querySelector(".modal-card");
  if (card && !card.contains(document.activeElement)) {
    (card.querySelector("button:not(.hidden):not(:disabled)") || card).focus?.();
  }
}

// ---- views ----
const VIEWS = ["main", "setup", "settings", "options", "autoexec", "whatsnew", "gv"];
function showView(name) {
  for (const v of VIEWS) $("view-" + v).classList.toggle("hidden", v !== name);
}
function currentView() {
  return VIEWS.find((v) => !$("view-" + v).classList.contains("hidden"));
}

// ---- busy: one owner at a time ----
// `busy` used to be a bare boolean that every flow set and cleared. Nothing stopped a second flow
// from starting (nothing checked before setting), and worse, whichever finished FIRST cleared the
// flag for both — so a `finally` from a quick check could unlock the whole UI while a multi-GB
// download was still streaming, re-enabling Play against a folder being rewritten. A token means
// only the flow that acquired the lock can release it.
let busySeq = 0;
function acquireBusy() {
  if (state.busy) return null;
  state.busy = true;
  state.busyOwner = ++busySeq;
  renderPrimary();
  return state.busyOwner;
}
function releaseBusy(token) {
  if (token == null || state.busyOwner !== token) return;
  state.busy = false;
  state.busyOwner = null;
  renderPrimary();
}

// ---- status ----
function setStatus(word, kind, detail) {
  $("status").dataset.kind = kind || "idle";
  $("status-word").textContent = word;
  const d = $("status-detail");
  if (detail instanceof Node) d.replaceChildren(detail);
  else d.textContent = detail || "";
}

// A detail line whose {version} renders as a mono pill (.dver) instead of plain text, so the
// installed release stands apart from the prose around it. The i18n string stays one template;
// the placeholder is interpolated as a sentinel and swapped for the chip here.
function detailVer(key, params) {
  const frag = document.createDocumentFragment();
  const parts = t(key, { ...params, version: "\x00" }).split("\x00");
  parts.forEach((p, i) => {
    if (i) {
      const c = document.createElement("span");
      c.className = "dver";
      c.textContent = params.version;
      frag.append(c);
    }
    if (p) frag.append(p);
  });
  return frag;
}

function setIdleStatus() {
  setStatus(t("status.notChecked"), "idle", t("status.checkHint"));
}

// A pending launcher update outranks every game status: until it is installed we can't trust
// that this build can even read the current manifest, so that is the only thing worth saying.
function launcherStatusParts() {
  const lu = state.launcherUpdate;
  return [
    t("status.launcherUpdate"),
    "update",
    t("detail.launcherUpdate", { from: lu.current, to: lu.version }),
  ];
}

function statusFor(v) {
  // A verdict from the install record alone (the network check failed). It knows whether our own
  // files are intact; it CANNOT know whether a newer release exists — so it never says "up to
  // date", and never wears the settled colour. Play stays available: being unable to ask about
  // updates is not a reason to make the game unplayable.
  if (v.local) {
    return [
      t("status.unverified"),
      "error",
      v.changes === 0
        ? detailVer("detail.localOk", { version: v.version })
        : detailVer("detail.localChanged", { version: v.version, n: v.changes }),
    ];
  }
  // No game to update: the folder has no game/ and no install record, so everything below —
  // "update available", the change count, Install — would describe installing a shim into
  // nothing. Say what is actually in the folder and offer the download (or its resume) instead.
  if (!v.gamePresent) {
    return [
      t("status.noGame"),
      "update",
      v.pendingBaseBytes > 0
        ? t("detail.resumeDl", { gb: (v.pendingBaseBytes / GB).toFixed(1) })
        : t("detail.noGame"),
    ];
  }
  if (v.changes === 0) {
    if (!v.installed) {
      // files all hash-match but no install state — the primary runs the no-op heal. Worded as
      // "repair", not "not installed": the list right below says every file is current, and
      // "Install" next to "all current" reads as a contradiction.
      return [t("status.repair"), "update", detailVer("detail.repair", { version: v.version })];
    }
    // The release has nothing pending — but files at our dests are somebody's own. Not an
    // error and not an update; a state that needs a decision, which is what Manage opens.
    if (v.userChanged > 0) {
      return [t("status.yourFiles"), "update", detailVer("detail.okMeta", { version: v.version })];
    }
    return [t("status.upToDate"), "ok", detailVer("detail.okMeta", { version: v.version })];
  }
  return [
    v.installed ? t("status.updateAvail") : t("status.notInstalled"),
    "update",
    detailVer("detail.changes", { version: v.version, n: v.changes }),
  ];
}

// ---- primary / buttons ----
function renderPrimary() {
  const p = $("btn-primary");
  const c = $("btn-check");
  // busy wins over "in game": a running op keeps everything locked either way
  if (state.busy) {
    // Every interruptible op now runs behind its own dialog with its own Stop (verify was the
    // last one that did not, and it borrowed this button). So the primary has nothing to offer
    // while an op holds the UI, and says so.
    p.textContent = t("status.working");
    p.disabled = true;
    c.disabled = true;
  } else if (state.gameRunning && state.primaryMode !== "check" && state.primaryMode !== "updateLauncher") {
    // the game is up: nothing that touches the GAME folder is offered (the backend interlock
    // backs this up); check stays available — it's read-only — and so does the launcher update,
    // which only renames the launcher's own exe. In those modes the primary is already the right
    // button, so they fall through to the normal branch and stay clickable.
    p.textContent = t("btn.ingame");
    p.disabled = true;
    c.disabled = false;
  } else {
    // apply splits three ways: heal (all files current, no install record) reads as Repair
    const heal = state.lastCheck && state.lastCheck.changes === 0 && !state.lastCheck.installed;
    const label = {
      check: "btn.check",
      play: "btn.play",
      updateLauncher: "btn.updateLauncher",
      downloadGame: state.lastCheck?.pendingBaseBytes > 0 ? "btn.resumeDl" : "btn.downloadGame",
      apply: heal ? "btn.repair" : state.lastCheck?.installed ? "btn.update" : "btn.install",
      // nothing to install — the release has no work here, only the user's own files do.
      // Saying "Update" over that claims a new version exists when none does.
      manage: "btn.manage",
    }[state.primaryMode];
    p.textContent = t(label);
    p.disabled = false;
    c.disabled = false;
  }
  // set from ONE place, outside the branches: a ghost left behind by a finished op would paint
  // the next Play/Install in the wrong weight entirely
  p.classList.remove("ghost"); // the primary is never the Stop any more — dialogs own that
  // the header refresh icon appears whenever the primary is something else
  c.classList.toggle("hidden", state.primaryMode === "check");

  $("btn-customize").disabled = state.busy;
  $("btn-settings").disabled = state.busy;
  $("btn-whatsnew").disabled = state.busy;
  // Save was the one control left live during an op: reachable behind a modal, and a save that
  // changes the folder/repo/token kicks off a check whose completion would fight the running flow
  $("btn-save").disabled = state.busy;
  // Settings' game-files tab: every control there mutates (or leads to mutating) the game folder,
  // so they share one lock — a repair rewrites mmapped VPKs, uninstall restores originals.
  // Uninstall stays VISIBLE with nothing to uninstall: inside a settings tab a control that
  // vanishes reads as a missing feature, while a disabled one reads as "not installed".
  const locked = state.busy || state.gameRunning;
  $("btn-verify").disabled = locked;
  $("btn-yours").disabled = locked;
  $("btn-fresh").disabled = locked;
  $("btn-uninstall").disabled = locked || !state.lastCheck?.canUninstall;
  renderLauncherUpdate();
}

// The notes surface for a pending self-update. Shown only when the release carries notes — the
// status line already names the versions, so a notes-less banner would say nothing. Hooked into
// renderPrimary (every flow's finally lands there), so it tracks state.launcherUpdate with no
// call sites of its own; the content is only re-rendered when the offered release changes, so a
// scroll position inside the notes box survives unrelated re-renders (busy toggles, game polls).
let luRenderedTag = null;
function renderLauncherUpdate() {
  const lu = state.launcherUpdate;
  const show = !!(lu && lu.notes);
  $("lu-banner").classList.toggle("hidden", !show);
  if (!show) { luRenderedTag = null; return; }
  if (luRenderedTag === lu.tag) return;
  luRenderedTag = lu.tag;
  $("lu-ver").textContent = `${lu.current} → ${lu.version}`;
  $("lu-notes").innerHTML = renderNotes(lu.notes);
}

// Both builds' versions on the bottom line (right of the game path), LABELED — a bare "v1.3.4"
// read as ambiguous (whose version?). The launcher's is known at boot. The mod's only when the
// folder is actually ON that version: a network check's `version` names the LATEST release, so
// with changes pending it would claim an update that isn't installed yet — a local verdict's
// names the install record itself, so it holds whatever the intact-check says.
// A tag carries a leading "v", a manifest's version usually doesn't, and launcher_info's never
// does — so nothing that COMPARES or PRINTS a version may take the producer's word for it. Shared
// by the foot line and the What's-new list: one version line, one spelling.
const bareVer = (s) => String(s).replace(/^v/i, "");
const sameVer = (a, b) => !!a && !!b && bareVer(a) === bareVer(b);

function renderHeadVer() {
  const vv = (s) => "v" + bareVer(s);
  const parts = [];
  if (state.launcherVersion) parts.push(t("head.launcherVer", { v: vv(state.launcherVersion) }));
  const c = state.lastCheck;
  if (c?.installed && c.version && (c.local || c.changes === 0))
    parts.push(t("head.modVer", { v: vv(c.version) }));
  $("app-ver").textContent = parts.join(" · ");
}

// Rebuild the managed-files list from a CheckView. Separate from applyCheck so a failed apply
// can reset half-filled bars / "N MB" states without touching the (error) status line.
// The managed-files list is TWO LEVELS: categories, each expandable to the files in it.
//
// A category is what a file IS, never what state it happens to be in:
//   - one per manifest option, the collapse this list already did ("New graphics × 33");
//   - **Phoenix core** for every shim file no option owns. Seven near-identical paths said
//     nothing at a glance and pushed the options off the screen;
//   - **Your files** for pins on dests the shim does not manage (`yours` from the backend) —
//     a vanilla file somebody modded. Those had no home here at all, so the only way to see a
//     decision the launcher was honouring on every check was a full game verification.
const FILES_CORE = "__core";
const FILES_YOURS = "__yours";

// One glyph per KIND of category, all in the same hairline weight (`.fgroup-ic`: 12px box, no
// fill, 1.2 stroke) so the column reads as one thing. They say what the row is, which is the only
// thing its label cannot: three categories that all look alike make the list a wall again.
//   layers   — an option's several files shown as one
//   nucleus  — the shim's own always-installed core (a ring around a centre: literally the core)
//   bookmark — files you told the launcher to keep
const FILES_CAT_IC = {
  option: '<path d="M6 1.5 10.5 4 6 6.5 1.5 4Z"/><path d="M1.5 6.5 6 9l4.5-2.5"/>',
  [FILES_CORE]: '<circle cx="6" cy="6" r="4.5"/><circle cx="6" cy="6" r="1.05"/>',
  [FILES_YOURS]: '<path d="M3.25 1.75h5.5v8.5L6 8.05 3.25 10.25z"/>',
};

function renderFiles(v) {
  state.filesShown = v; // what a category toggle re-renders, without re-running a check
  const ul = $("files");
  ul.innerHTML = "";
  state.fileEls.clear();
  // How actionable each state is — a collapsed category shows its most actionable member. `kept`
  // used to tie with `ok`, on the grounds that it is a decision already made; as a SUMMARY that
  // reads as a claim, and "current" over a category holding one of the user's own files is the
  // same lie as an "all current" line printed above a modified row. It ranks above `ok` and below
  // everything the release would act on.
  const rank = { ok: 0, kept: 1, remove: 2, install: 3, update: 4, modified: 5 };
  // dest -> every row describing it (the category row, plus its child row while expanded). Both
  // get the live download bar; only the category row carries `_pending`, so it still settles on
  // its LAST member while each child settles on its own.
  const bind = (dest, li) => {
    const cur = state.fileEls.get(dest);
    if (cur) cur.push(li);
    else state.fileEls.set(dest, [li]);
  };
  const mkRow = (main, status, upd, cls) => {
    const li = document.createElement("li");
    li.className = cls;
    const st = document.createElement("span");
    st.className = "fstate " + status;
    st.textContent = t("fstate." + status) + (upd ? " / " + t("fstate.updateAvail") : "");
    // files that will be fetched (update/install) carry a hairline bar, revealed + filled live
    // from op-progress ticks during apply (downloads run in parallel — one bar each)
    const bar = document.createElement("span");
    bar.className = "fbar";
    bar.innerHTML = '<span class="fbar-fill"></span>';
    li.append(main, st, bar);
    ul.append(li);
    return li;
  };

  // bucket first, in manifest order — both the categories and the files inside them
  const cats = new Map();
  for (const f of v.files) {
    const id = f.groupId || (f.yours ? FILES_YOURS : FILES_CORE);
    let c = cats.get(id);
    if (!c) {
      c = { id, option: !!f.groupId, group: f.group, variant: f.variant, files: [] };
      cats.set(id, c);
    }
    c.files.push(f);
  }

  for (const c of cats.values()) {
    const open = state.filesOpen.has(c.id);
    const status = c.files.reduce((a, m) => (rank[m.status] > rank[a] ? m.status : a), "ok");
    const name = document.createElement("span");
    name.className = "fgroup";
    name.innerHTML =
      '<svg class="fchev" viewBox="0 0 12 12" aria-hidden="true"><path d="M4.5 2 8.5 6l-4 4"/></svg>' +
      '<svg class="fgroup-ic" viewBox="0 0 12 12" aria-hidden="true">' +
      (c.option ? FILES_CAT_IC.option : FILES_CAT_IC[c.id]) +
      "</svg>";
    const label = document.createElement("span");
    label.className = "fgroup-name";
    label.textContent = c.option
      ? mlabel(c.group) || c.id
      : t(c.id === FILES_YOURS ? "files.catYours" : "files.catCore");
    name.append(label);
    // a choice's row names the SELECTED variant, not the shared dest ("Lighting · Mod")
    if (c.variant) {
      const va = document.createElement("span");
      va.className = "fvariant";
      va.textContent = mlabel(c.variant);
      name.append(va);
    }
    if (c.files.length > 1) {
      const count = document.createElement("span");
      count.className = "fcount";
      count.textContent = "× " + c.files.length; // language-free on purpose
      name.append(count);
    }
    const li = mkRow(name, status, c.files.some((m) => m.updateAvailable), "fcat" + (open ? " open" : ""));
    li.dataset.cat = c.id;
    li.setAttribute("role", "button");
    li.setAttribute("aria-expanded", String(open));
    li.tabIndex = 0;
    // The row goes green only when EVERY downloading member is done, not the first — so this must
    // count what will actually be written, which in a partial restore includes the `modified`
    // members that were ticked. Counting update/install alone let the first completion settle the
    // whole category while two of its files were still streaming.
    li._pending = c.files.filter((m) => state.applyWrites.has(m.dest) ||
      m.status === "update" || m.status === "install").length;
    for (const f of c.files) bind(f.dest, li);
    if (!open) continue;
    for (const f of c.files) {
      const path = document.createElement("span");
      path.className = "fpath";
      path.textContent = f.dest;
      const kid = mkRow(path, f.status, f.updateAvailable, "fkid");
      kid.dataset.dest = f.dest;
      bind(f.dest, kid);
    }
  }
  $("files-empty").style.display = v.files.length ? "none" : "flex";
  // every row that is the user's own doing, whichever way it got that way
  const yours = v.files.filter((f) => f.status === "modified" || f.status === "kept").length;
  // `changes` counts what the RELEASE would change, which is no longer the whole list: a folder
  // with nothing pending but two of the user's own files at our dests is not "all current", and
  // saying so directly contradicts the rows underneath.
  $("files-count").textContent = !v.files.length
    ? "" // an empty (cleared) list isn't "all current" — it's not read yet
    : v.changes > 0
    ? t("files.toChange", { n: v.changes })
    // `userChanged` counts `Modified` ONLY — deliberately, because the same number drives the
    // apply confirm and a pin is not something apply is about to overwrite. It is the wrong number
    // for a LABEL over these rows: one pinned file made this line read "all current" directly
    // above a category whose own state (correctly) said "kept". Count what is rendered.
    : yours > 0
    ? t("files.yours", { n: yours })
    : t("files.allCurrent");
}

function applyCheck(v) {
  state.lastCheck = v;
  renderHeadVer(); // the mod half of the header line comes from this check
  // a check completing while the game runs must not overwrite the "in game" status
  // launcher update FIRST, then "in game". The order is the documented invariant: until the
  // launcher is replaced we can't trust that this build reads the current manifest at all, and a
  // self-update touches nothing but the launcher's own exe — so a running game is no reason to
  // hide it (or, as the reversed order did, to paint it in the settled colour behind a dead
  // "In game" button that offered no way to install it).
  let [word, kind, detail] = state.launcherUpdate
    ? launcherStatusParts()
    : state.gameRunning
    ? [t("status.ingame"), "ok", t("detail.ingame")]
    : statusFor(v);
  // one-shot confirmation that a self-update landed — kept ALONGSIDE the real status rather than
  // replacing it, since the game's state is still the more useful half of the line
  if (state.justUpdated) {
    // detail may be a Node now (the version chip) — prepend via a fragment, not string concat
    const f = document.createDocumentFragment();
    f.append(t("detail.justUpdated", { version: state.launcherVersion }) + " · ",
             detail instanceof Node ? detail : detail || "");
    detail = f;
    state.justUpdated = false;
  }
  // Phoenix files somebody has changed. Apply will overwrite them (a corrupt shim file has to be
  // repairable by the button whose job that is), so the one thing that must not happen is for
  // them to go unmentioned. Folded into `detail` BEFORE the single setStatus call: appending to a
  // DocumentFragment that has already been inserted moves nothing, because inserting a fragment
  // empties it — the version chip silently disappeared from the line.
  if (v.userChanged > 0 && !state.launcherUpdate) {
    const f = document.createDocumentFragment();
    f.append(detail instanceof Node ? detail : detail || "",
             " · " + t("status.userChanged", { n: v.userChanged }));
    detail = f;
  }
  setStatus(word, kind, detail);

  // an empty list, not the shim manifest's diff: rows of "install" pending under a "no game
  // here" status read as two contradicting verdicts about one folder
  renderFiles(v.gamePresent ? v : { files: [], changes: 0 });

  const pl = $("game-path");
  pl.textContent = v.gameDir;
  pl.title = v.gameDir;

  // always offered once checked: the history view serves older releases' notes even when the
  // latest release carries none (the backend is built for exactly that case)
  $("btn-whatsnew").classList.remove("hidden");
  $("btn-customize").classList.toggle("hidden", !(v.options && v.options.length) || !v.gamePresent);

  // a pending launcher update takes the primary: this build may not be able to read the current
  // manifest at all, so replacing it comes before installing anything described by that manifest.
  // Then "no game": with nothing to install into, the download IS the next step — as the primary,
  // not an error message pointing at a settings tab.
  state.primaryMode = state.launcherUpdate
    ? "updateLauncher"
    : !v.gamePresent ? "downloadGame"
    : v.primaryAction === "apply" ? "apply"
    : v.primaryAction === "manage" ? "manage"
    : v.canPlay ? "play" : "check";
  renderPrimary();
}

// Command failures arrive as {kind, message} envelopes (CmdError); tolerate bare strings too.
function errText(e) {
  return (e && typeof e === "object" && "message" in e) ? e.message : String(e);
}

// The status word reacts to the error kind (offline / access / outdated launcher / game
// running); the mono detail keeps a localized hint plus the raw engine message.
const ERR_WORDS = { network: "status.offline", auth: "status.noAccess", tooOld: "status.tooOld", gameRunning: "status.gameLocked" };
const ERR_HINTS = { network: "err.network", auth: "err.auth", tooOld: "err.tooOld", gameRunning: "err.gameRunning" };
function onError(e) {
  const kind = e && typeof e === "object" ? e.kind : null;
  const word = t(ERR_WORDS[kind] || "status.error");
  const hint = ERR_HINTS[kind] ? t(ERR_HINTS[kind]) + " · " : "";
  setStatus(word, "error", hint + errText(e));
}

// ---- actions ----
async function doCheck() {
  const busy = acquireBusy();
  if (busy == null) return;
  setStatus(t("status.working"), "busy", t("detail.reading"));
  // The launcher checks ITSELF on the same trip — different repo, so the two run in parallel.
  // allSettled, not all: both must have landed before we decide what to paint, and checkLauncher
  // never rejects, so a launcher-side failure can't turn a good game check into an error.
  const [, game] = await Promise.allSettled([checkLauncher(), invoke("check")]);
  try {
    if (game.status === "rejected") throw game.reason;
    applyCheck(game.value);
  } catch (e) {
    onError(e);
    // a pending launcher update is more actionable than a failed game check — and may well be
    // its cause (a manifest this build can't read). Say that instead.
    if (state.launcherUpdate) {
      state.primaryMode = "updateLauncher";
      setStatus(...launcherStatusParts());
    } else {
      await fallBackToLocal(e);
    }
  } finally {
    releaseBusy(busy);
  }
}

// A failed check must not leave the launcher useless. Play and Uninstall are entirely local, and
// `lastCheck` is what unlocks them — so when the network verdict is unavailable, ask the install
// record instead. The result is worded as what it is (we could not check), never as "up to date":
// it describes the files WE installed, not the latest release.
async function fallBackToLocal(cause) {
  let v;
  try {
    v = await invoke("local_check");
  } catch (_) {
    return; // nothing installed to fall back to — the original error stands
  }
  // applyCheck words a local verdict for itself (statusFor), so it stays correct through a
  // language switch or a game-close refresh too; only the reason is appended here
  applyCheck(v);
  // append, don't `textContent +=` — that would flatten the version chip back into plain text
  $("status-detail").append(" · " + errText(cause));
}

async function doReplan() {
  try {
    applyCheck(await invoke("replan"));
  } catch (e) {
    onError(e);
  }
}

/// `restore` narrows the run to named Phoenix dests — the files view restoring files somebody
/// changed. Omitted, this is the ordinary apply, which acts on every unattended difference and
/// never touches a pinned one.
async function doApply(restore) {
  // An ordinary apply repairs Phoenix files somebody changed — which is right (a corrupt shim
  // file must be fixable by the button whose whole job is fixing shim files) and destructive. It
  // used to ask with a yes/no confirm, which is the wrong shape of question: the answer is rarely
  // "all of them" or "none of them". Instead the button opens a MENU of exactly those files, and
  // the release's other changes are installed either way. A caller that already passed a selection
  // has been through this (or through verify's own restore) and must not be asked twice.
  if (!restore && state.lastCheck && state.lastCheck.userChanged > 0) {
    openUpdateMenu(state.lastCheck);
    return;
  }
  const busy = acquireBusy();
  if (busy == null) return;
  setStatus(t("status.working"), "busy", t("detail.installing"));
  // the engine streams phase-1 progress as op-progress events; downloads run in parallel, so
  // ticks for different files interleave. Each file's own bar (keyed by dest in state.fileEls)
  // fills from its byte ticks. The header counts DESTS done, not the engine's unique-asset
  // current/total — dests are what the visible rows are, so the numbers always match the list.
  // The denominator has to describe THIS run's work set, not the release's. A restore also writes
  // the `modified` dests it was handed, so counting only update/install rows made the line count
  // past its own total — "5 of 2 files" for two pending updates plus three files put back.
  state.applyWrites = new Set(restore || []);
  const willWrite = (f) =>
    f.status === "update" || f.status === "install" || state.applyWrites.has(f.dest);
  const dlTotal = state.lastCheck ? state.lastCheck.files.filter(willWrite).length : 0;
  // the rows were built before this run knew its selection — rebuild so each category waits for
  // the members it will actually receive
  if (state.applyWrites.size && state.filesShown) renderFiles(state.filesShown);
  const doneDests = new Set();
  let unlisten = null;
  try {
    unlisten = await listen("op-progress", (ev) => {
      const p = ev.payload;
      if (p.op !== "install" || !p.item) return;
      // One dest can be on screen TWICE — its category row and, while that category is expanded,
      // its own child row. Both track the same download.
      const rows = state.fileEls.get(p.item);
      if (!rows) return;
      if (p.done) doneDests.add(p.item);
      for (const li of rows) {
        li.classList.add("dl");
        const fill = li.querySelector(".fbar-fill");
        const st = li.querySelector(".fstate");
        if (p.done) {
          // a category row waits for its LAST downloading member before settling (li._pending is
          // the member count from renderFiles; child rows have none and settle at once)
          if (li._pending != null && --li._pending > 0) {
            st.className = "fstate dl";
          } else {
            li.classList.add("done");
            if (fill) fill.style.width = "100%";
            st.className = "fstate ok";
            st.textContent = t("fstate.ok");
          }
        } else if (p.bytesTotal) {
          const pct = Math.min(100, (p.bytesDone / p.bytesTotal) * 100);
          if (fill) fill.style.width = pct.toFixed(1) + "%";
          st.className = "fstate dl";
          // localized like every other size string — this was the one hardcoded "MB", sitting four
          // rows under a status line that says "МБ"
          st.textContent = t("fstate.dlSize", {
            done: (p.bytesDone / 1048576).toFixed(1),
            total: (p.bytesTotal / 1048576).toFixed(1),
          });
        }
      }
      setStatus(t("status.working"), "busy", t("detail.dl", { i: doneDests.size, n: dlTotal || p.total }));
    });
    // pinned to the release this button is DESCRIBING (state.lastCheck) — same rule as the
    // launcher self-update: what the button offers is what the button installs. A local (offline)
    // verdict carries no tag and never offers apply, so null only means "no prior check".
    const tag = state.lastCheck && !state.lastCheck.local && state.lastCheck.tag
      ? state.lastCheck.tag : null;
    await invoke("apply", { tag, restore: restore || null });
    // no network: apply refreshed the backend's manifest cache from the release it installed
    await doReplan();
  } catch (e) {
    onError(e);
    // reset half-filled bars / "N MB" states to the last known plan (the status line keeps
    // showing the error — renderFiles doesn't touch it)
    if (state.lastCheck) renderFiles(state.lastCheck);
  } finally {
    if (unlisten) unlisten();
    // this run's selection is spent — a later render must not go on waiting for dests nobody is
    // writing any more
    state.applyWrites = new Set();
    releaseBusy(busy);
  }
}

async function doUninstall() {
  const busy = acquireBusy();
  if (busy == null) return;
  setStatus(t("status.working"), "busy", t("detail.reverting"));
  try {
    await invoke("uninstall");
    // replan, not check: uninstall itself is fully offline — its result must not turn into a
    // network error status when the connection happens to be down
    await doReplan();
  } catch (e) {
    onError(e);
  } finally {
    releaseBusy(busy);
  }
}

// ---- launcher self-update ----
// Errors are swallowed on purpose and reported as a boolean: a failed check means UNKNOWN, and
// `state.launcherUpdate` keeps whatever it held. Collapsing "couldn't ask" into "nothing to
// install" is the one mistake that would let a stale launcher through the Play gate.
async function checkLauncher() {
  try {
    state.launcherUpdate = await invoke("launcher_check");
    return true;
  } catch (e) {
    return false;
  }
}

async function doLauncherUpdate() {
  const busy = acquireBusy();
  if (busy == null) return;
  setStatus(t("status.working"), "busy", t("detail.launcherDl", { pct: 0 }));
  let unlisten = null;
  try {
    unlisten = await listen("launcher-progress", (ev) => {
      const p = ev.payload;
      setStatus(t("status.working"), "busy", p.bytesTotal
        ? t("detail.launcherDl", { pct: Math.min(100, (p.bytesDone / p.bytesTotal) * 100).toFixed(0) })
        : t("detail.launcherDlSize", { mb: (p.bytesDone / 1048576).toFixed(1) }));
    });
    // pinned to the release the banner is showing — see the command's own note on why
    // re-resolving "latest" at install time is not the same thing
    await invoke("launcher_update", { tag: state.launcherUpdate?.tag });
    // the swap succeeded and the backend is exiting into the new binary — stay busy, since
    // there is nothing left for this window to do
    setStatus(t("status.restarting"), "busy", t("detail.restarting"));
  } catch (e) {
    if (e && e.kind === "restartFailed") {
      // The swap DID happen — the new build is already on disk under the launcher's name and
      // only the relaunch failed. Saying "update failed" would be false and would send the user
      // round the same loop; clearing the pending update stops the primary re-offering it.
      state.launcherUpdate = null;
      setStatus(t("status.restartNeeded"), "update", t("detail.restartNeeded") + " · " + errText(e));
    } else {
      // nothing was replaced: apply() verifies before it swaps, and rolls the rename back if the
      // second one fails. The running launcher is still intact, so hand control back.
      onError(e);
    }
    releaseBusy(busy);
  } finally {
    if (unlisten) unlisten();
  }
}

// ---- play ----
// Play is a GATE, not just a launch: a "up to date" verdict from an hour ago must not put an
// outdated client on a server. Order matters — the launcher is verified FIRST, because a newer
// release can ship a manifest format this build cannot read, and checking the game first would
// only dead-end on that (`tooOld`).
//
// Being unable to verify is not the same as being outdated. When the checks fail for these
// reasons we launch anyway and say so in the detail line: Play is only reachable when the last
// known state was installed and clean, and GitHub being unreachable — or the dist release being
// pulled/renamed (notFound) — must not make the game unplayable. A check that SUCCEEDS and
// reports work to do still blocks.
const SOFT_ERR = new Set(["network", "auth", "notFound"]);

async function doPlay() {
  const busy = acquireBusy();
  if (busy == null) return;
  // a verify said these game files are damaged and the user declined the repair — launching would
  // start the client the launcher just called broken
  if (state.gameDamaged) {
    setStatus(t("status.gvDamaged"), "error", t("gv.playBlocked", { n: state.gameDamaged }));
    releaseBusy(busy);
    return;
  }
  setStatus(t("status.working"), "busy", t("detail.verifying"));
  let unverified = null;
  try {
    // both verifications in flight AT ONCE (different repos) — Play's latency is the slower of
    // the two, not their sum, which matters most offline where both are eating a connect
    // timeout. The launcher verdict still gates FIRST below: a newer release can ship a
    // manifest format this build cannot read, so the game result must not be acted on before
    // the launcher one. checkLauncher never rejects (it reports a boolean), so lu.value is safe.
    const [lu, game] = await Promise.allSettled([checkLauncher(), invoke("check")]);
    // paint whatever the game check learned — applyCheck sees the fresh launcherUpdate state
    // (both are settled) and words/arms the UI for it by itself
    if (game.status === "fulfilled") applyCheck(game.value);
    if (lu.value !== true) unverified = t("detail.unverified");

    if (state.launcherUpdate) {
      // verified-outdated launcher: blocked. The game check ran too — one fetch of waste in the
      // rare blocked case buys the parallel fast path everywhere else.
      state.primaryMode = "updateLauncher";
      if (game.status !== "fulfilled") setStatus(...launcherStatusParts());
      return;
    }

    if (game.status === "rejected") {
      const e = game.reason;
      // tooOld here means the manifest wants a launcher newer than any release we could find —
      // onError says exactly that, and there is nothing to offer beyond it
      if (!SOFT_ERR.has(e && e.kind)) { onError(e); return; }
      unverified = t("detail.unverified");
    } else if (game.value.changes > 0 || !game.value.installed) {
      setStatus(t("status.updateRequired"), "update", t("detail.updateRequired"));
      return;
    }

    await invoke("play");
    setStatus(t("status.launched"), "ok", unverified || t("detail.launched"));
  } catch (e) {
    onError(e);
  } finally {
    releaseBusy(busy);
  }
}

function onPrimary() {
  if (state.busy) return;
  if (state.primaryMode === "apply") doApply();
  // "Manage files" opens the FULL answer, not the shim half of it. It used to open the update
  // menu over `lastCheck`, which knows only the shim's own dests — so a button sitting under a
  // list that now shows a "Your files" category could not show the files in it, and the menu it
  // did open worded itself as a release's update ("this release replaces N files") when the
  // release, by the definition of this mode, has nothing pending at all.
  // Safe on the network: `manage` is only ever set by a SUCCESSFUL check — the offline verdict
  // leaves the primary on Check — so the fetch this needs is one the app just made.
  else if (state.primaryMode === "manage") doYourFiles("main");
  else if (state.primaryMode === "play") doPlay();
  else if (state.primaryMode === "updateLauncher") doLauncherUpdate();
  else if (state.primaryMode === "downloadGame") startGameResume();
  else doCheck();
}

// ---- game tracking ----
// Poll the game process; transitions drive the status: "in game" while it runs (Play and
// Uninstall locked), and one re-plan when it closes — files may have changed (a patch, a
// verify, a crash mid-write).
async function pollGame() {
  let running;
  try {
    running = await invoke("game_running");
  } catch (e) {
    return; // keep the previous state; next tick retries
  }
  if (running === state.gameRunning) return;
  state.gameRunning = running;
  renderPrimary();
  if (running) {
    // never stomp a running op's status line; it re-renders on its next tick anyway
    if (!state.busy) setStatus(t("status.ingame"), "ok", t("detail.ingame"));
  } else if (!state.busy) {
    // re-diff locally against the cached manifest: no network (closing the game while offline
    // must not flip "Up to date" into a network error) and no busy flicker. Without a prior
    // check there is nothing cached — fall back to the full check.
    if (state.lastCheck) doReplan();
    else doCheck();
  }
}

// ---- language ----
function rerenderDynamic() {
  applyStatic();
  renderHeadVer(); // labeled, so it re-renders with the language
  // the files view holds live state (a selection, an open tree) — re-word it in place rather
  // than rebuilding it, which would throw the user's ticks away
  if (gv.data) { renderGvChrome(); renderGvFacets(); gvRebuild(); }
  if (state.lastCheck) applyCheck(state.lastCheck);
  else { setIdleStatus(); renderPrimary(); }
  updateTokenPlaceholder();
  renderLaunchFlags();
  renderOptions();
  // The What's-new panes are re-worded by DISCARDING them: their only localized part is the
  // "current" pill, and they are rebuilt from a command anyway. Re-wording in place (what the
  // files view does, because it holds a selection) would buy nothing, while leaving them alone
  // left a pill in the old language whenever the next open's refetch failed.
  for (const k in WN) WN[k].loaded = false;
}

async function switchLang(l) {
  setLang(l);
  setSeg($("seg-lang"), l);
  rerenderDynamic();
  try { await invoke("set_language", { language: l }); } catch (e) { /* non-fatal */ }
}

// The animations master switch: one root class the CSS keys a blanket kill off of. The class is
// the OFF state so a stub/boot failure fails toward animations on (the default).
function applyAnimations(on) {
  document.documentElement.classList.toggle("anim-off", !on);
}

// ---- segmented controls ----
function setSeg(seg, value) {
  for (const b of seg.querySelectorAll(".seg-btn")) b.classList.toggle("active", b.dataset.value === value);
}
function segValue(seg) {
  return seg.querySelector(".seg-btn.active")?.dataset.value;
}
function wireSeg(seg, onPick) {
  seg.addEventListener("click", (e) => {
    const b = e.target.closest(".seg-btn");
    if (!b) return;
    setSeg(seg, b.dataset.value);
    if (onPick) onPick(b.dataset.value);
  });
}

// ---- settings ----
// Tabs are pure display: every field stays in the DOM whichever pane is shown, so the snapshot
// guard and Save keep seeing one whole form.
function setSettingsTab(name) {
  state.settingsTab = name;
  for (const b of $("settings-tabs").querySelectorAll(".tab")) {
    const on = b.dataset.tab === name;
    b.classList.toggle("active", on);
    b.setAttribute("aria-selected", String(on));
  }
  for (const p of $("settings-panels").querySelectorAll("[data-panel]"))
    p.classList.toggle("hidden", p.dataset.panel !== name);
  $("settings-panels").scrollTop = 0; // the panes share one scroller — never inherit an offset
}

function setSettingsMsg(text) {
  const m = $("settings-msg");
  if (text) { m.textContent = text; m.hidden = false; } else { m.hidden = true; }
}

function updateTokenPlaceholder() {
  $("in-token").placeholder = state.tokenClear
    ? t("ph.tokenCleared")
    : state.hasToken ? t("ph.tokenSaved") : t("ph.tokenEmpty");
}

// The optional launch flags (backend table, one plate each — the whole plate is the switch, so
// the hit target is the row). The label is `set.flag.<id>`; a flag with no string yet falls back
// to its raw args, so a new backend row is still usable.
function renderLaunchFlags() {
  const list = $("flag-list");
  list.innerHTML = "";
  // .hidden, not the attribute — `.field`'s display:flex would win over [hidden]
  $("launch-flags").classList.toggle("hidden", state.launchFlags.length === 0);
  for (const f of state.launchFlags) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "flag-row" + (f.enabled ? " on" : "");
    row.setAttribute("role", "switch");
    row.setAttribute("aria-checked", String(f.enabled));

    const text = document.createElement("span");
    text.className = "flag-text";
    const name = document.createElement("span");
    name.className = "flag-name";
    const key = "set.flag." + f.id;
    const label = t(key);
    name.textContent = label === key ? f.args : label;
    const args = document.createElement("span");
    args.className = "flag-args";
    args.textContent = f.args;
    text.append(name, args);

    const sw = document.createElement("span");
    sw.className = "switch" + (f.enabled ? " on" : "");
    sw.setAttribute("aria-hidden", "true");

    row.addEventListener("click", () => {
      f.enabled = !f.enabled;
      row.classList.toggle("on", f.enabled);
      sw.classList.toggle("on", f.enabled);
      row.setAttribute("aria-checked", String(f.enabled));
    });

    row.append(text, sw);
    list.append(row);
  }
}

// The form's current content, for the discard-changes guard. Language is excluded — it applies
// (and persists) instantly on toggle, so it is never "unsaved".
function settingsSnapshot() {
  return JSON.stringify({
    repo: $("in-repo").value,
    game: $("in-game").value,
    launch: $("in-launch").value,
    renderer: segValue($("seg-renderer")),
    flags: state.launchFlags.map((f) => [f.id, f.enabled]),
    token: $("in-token").value,
    clear: state.tokenClear,
  });
}

async function openSettings() {
  let s;
  try {
    s = await invoke("get_settings");
  } catch (e) {
    onError(e); // stay on main, surface the failure instead of swallowing it
    return;
  }
  $("in-repo").value = s.sourceRepo || "";
  $("in-game").value = s.gameDir || "";
  $("in-token").value = "";
  $("in-launch").value = s.launchExtra || "";
  state.hasToken = s.hasToken;
  state.tokenClear = false;
  state.renderer = s.renderer || "dx11";
  state.launchFlags = (s.launchFlags || []).map((f) => ({ ...f }));
  renderLaunchFlags();
  updateTokenPlaceholder();
  $("btn-token-clear").classList.toggle("hidden", !state.hasToken);
  setSeg($("seg-renderer"), state.renderer);
  setSeg($("seg-lang"), LANG);
  setSeg($("seg-anim"), s.animations === false ? "off" : "on");
  $("advanced").classList.toggle("hidden", !SHOW_ADVANCED);
  $("advanced").open = false;
  setSettingsTab(state.settingsTab);
  setSettingsMsg(null);
  state.settingsLoaded = { repo: s.sourceRepo || "", game: s.gameDir || "" };
  state.settingsSnap = settingsSnapshot();
  showView("settings");
}

async function saveSettings() {
  try {
    await invoke("save_settings", {
      sourceRepo: $("in-repo").value,
      gameDir: $("in-game").value,
      token: $("in-token").value,
      clearToken: state.tokenClear,
      language: LANG,
      launchExtra: $("in-launch").value,
      renderer: segValue($("seg-renderer")) || "dx11",
      launchFlags: Object.fromEntries(state.launchFlags.map((f) => [f.id, f.enabled])),
    });
    // a save that changes where updates come from (folder, repo, credentials) makes every bit
    // of the shown state stale — the status, the file list, and above all Play, which would
    // launch the NEW folder while the UI still describes the old one. Invalidate and re-check.
    const dirChanged = $("in-game").value.trim() !== state.settingsLoaded.game.trim();
    const invalidates =
      dirChanged ||
      $("in-repo").value.trim() !== state.settingsLoaded.repo.trim() ||
      $("in-token").value !== "" || state.tokenClear;
    // the damaged-files verdict describes ONE folder — pointing at another must release it (a
    // repo/token change keeps it: same folder, same files). The old verdict otherwise blocked
    // Play forever, counting files in a folder the launcher no longer even looks at.
    if (dirChanged) state.gameDamaged = 0;
    if ($("in-token").value) state.hasToken = true;
    if (state.tokenClear) state.hasToken = false;
    state.tokenClear = false;
    state.settingsSnap = null;
    showView("main");
    if (invalidates) {
      state.lastCheck = null;
      state.primaryMode = "check";
      renderFiles({ files: [], changes: 0 });
      $("game-path").textContent = "";
      $("btn-customize").classList.add("hidden");
      $("btn-whatsnew").classList.add("hidden");
      doCheck();
    } else {
      setStatus(t("status.saved"), "ok", "");
    }
  } catch (e) {
    setSettingsMsg(errText(e));
  }
}

// Back/Escape out of settings: same protection the autoexec editor has — unsaved edits ask
// before being dropped.
/// Leave settings, asking first if the form is dirty. Returns false when the user kept editing —
/// callers that do something AFTER closing (Verify, which runs on main) must not proceed then.
async function maybeCloseSettings() {
  if (state.settingsSnap !== null && settingsSnapshot() !== state.settingsSnap) {
    const ok = await confirmDialog({
      title: t("cf.discardTitle"),
      text: t("cf.discardSettingsText"),
      confirm: t("cf.discardConfirm"),
    });
    if (!ok) return false;
  }
  state.tokenClear = false;
  state.settingsSnap = null;
  showView("main");
  return true;
}

// Locating an EXISTING install. The title says so — the same picker also serves "where should the
// download go", where "the folder that contains game\" would be a lie.
async function browseInto(input) {
  try {
    const dir = await invoke("browse_folder", { title: t("dlg.pickGame"), start: input.value });
    if (dir) input.value = dir;
  } catch (e) {
    setSettingsMsg(errText(e));
  }
}

// ---- setup (first run) ----
function setSetupMsg(text) {
  const m = $("setup-msg");
  if (text) { m.textContent = text; m.hidden = false; } else { m.hidden = true; }
}

async function adoptGameDir(path) {
  try {
    await invoke("set_game_dir", { path });
  } catch (e) {
    // If the autofind modal is up (this is reached from its "Use" buttons), the setup view's
    // plate is BEHIND it — writing there made a failed pick look like a dead button. The modal
    // has its own plate for exactly this.
    if (!$("af-modal").classList.contains("hidden")) {
      const m = $("af-msg");
      m.textContent = errText(e);
      m.classList.remove("hidden");
    } else {
      setSetupMsg(errText(e));
    }
    return;
  }
  closeAutofind();
  // a different folder: whatever a verify said about the OLD one no longer describes anything
  // this UI points at — without this, "N files damaged" from folder A blocked Play in folder B
  state.gameDamaged = 0;
  showView("main");
  setIdleStatus();
  doCheck();
}

async function setupBrowse() {
  try {
    const dir = await invoke("browse_folder", { title: t("dlg.pickGame") });
    if (dir) adoptGameDir(dir); // any folder is accepted
  } catch (e) {
    setSetupMsg(errText(e));
  }
}

// ---- autofind ----
let afSeq = 0; // drops a cancelled scan's results when the dialog was reopened meanwhile

function afStage(stage) {
  for (const s of ["af-warn", "af-run", "af-results"]) $(s).classList.toggle("hidden", s !== "af-" + stage);
}

function openAutofind(target) {
  state.afTarget = target;
  afStage("warn");
  $("af-modal").classList.remove("hidden");
}

function closeAutofind() {
  $("af-modal").classList.add("hidden");
  if (state.afUnlisten) { state.afUnlisten(); state.afUnlisten = null; }
}

async function runAutofind() {
  // A cancelled scan keeps running until the backend's walk notices the flag, so the dialog can
  // be reopened while one is still in flight. Showing the RUN stage (rather than returning with
  // no feedback, which read as a dead Continue button) both explains the wait and lets the
  // sequence check below decide whose results the dialog gets.
  if (state.afBusy) {
    afStage("run");
    $("af-count").textContent = t("af.scanning");
    return;
  }
  state.afBusy = true;
  const seq = ++afSeq;
  afStage("run");
  $("af-count").textContent = t("af.scanning");
  $("af-current").textContent = "";
  if (state.afUnlisten) { state.afUnlisten(); state.afUnlisten = null; } // never leak a listener
  state.afUnlisten = await listen("autofind-progress", (ev) => {
    const p = ev.payload;
    $("af-count").textContent = t("af.scanned", { n: p.scanned });
    $("af-current").textContent = p.current;
  });
  let found = null; // null = the scan itself failed (≠ an empty result)
  let err = null;
  try {
    found = await invoke("autofind_start");
  } catch (e) {
    err = e;
  }
  state.afBusy = false;
  if (state.afUnlisten) { state.afUnlisten(); state.afUnlisten = null; }
  // the modal was closed mid-scan (Escape) — discard the results instead of staging them
  // under a hidden modal
  if ($("af-modal").classList.contains("hidden")) return;
  // ...and a scan the user cancelled must not take over a dialog that has since been reopened:
  // its partial results would appear as if the new run had produced them
  if (seq !== afSeq) return;
  renderCandidates(found || []);
  // a failed scan says WHY instead of masquerading as "Nothing found"
  const msg = $("af-msg");
  msg.classList.toggle("hidden", !err);
  if (err) {
    msg.textContent = errText(err);
    $("cands-empty").classList.add("hidden");
  }
  afStage("results");
}

function renderCandidates(found) {
  const ul = $("cands");
  ul.innerHTML = "";
  $("cands-empty").classList.toggle("hidden", found.length > 0);
  for (const c of found) {
    const li = document.createElement("li");
    const info = document.createElement("div");
    info.className = "cand-info";
    const p = document.createElement("div");
    p.className = "cand-path";
    p.textContent = c.path;
    p.title = c.path;
    const v = document.createElement("div");
    v.className = "cand-build";
    v.textContent = t("setup.build", { v: c.clientVersion || "?" });
    info.append(p, v);
    const use = document.createElement("button");
    use.className = "btn ghost inline";
    use.textContent = t("setup.use");
    use.addEventListener("click", () => {
      if (state.afTarget === "settings") {
        $("in-game").value = c.path;
        closeAutofind();
      } else {
        adoptGameDir(c.path);
      }
    });
    li.append(info, use);
    ul.append(li);
  }
}

function cancelAutofind() {
  invoke("autofind_cancel").catch(() => {});
  // Stop button: the running scan returns what it found so far; results stage follows from
  // runAutofind(). (Escape cancels AND closes вЂ” runAutofind then discards the results.)
}

// ---- base game: fresh download / verify / repair ----
// One modal serves download and repair — both are the same backend pipeline (plan-diff, then
// fetch what's missing), so the UI difference is only the title and whether a confirm step runs.
// Progress is ONE aggregate bar: the payload is gigabytes across hundreds of files, and per-file
// rows at that scale are noise, not information.
const GB = 1024 * 1024 * 1024;
const gd = {
  mode: null,      // "install" | "repair"
  origin: null,    // "setup" | "settings" — where to land after a successful install
  dir: null,
  bytes: 0,        // planned unique download bytes (the bar's full extent)
  files: 0,        // planned file count
  restore: [],     // repair only: the dests the user checked
  keep: [],        // repair only: the differences they are deliberately leaving alone
  thenPhoenix: null, // repair only: the same selection's shim half, run after the game half lands
  perFile: null,   // Map dest -> bytesDone (ticks fan out per dest; clamp the sum to `bytes`)
  sum: 0,          // running total of perFile's values, maintained by delta (never re-summed)
  phase: "plan",   // "plan" -> "fetch": which half of the run the line describes (onGdProgress)
  doneFiles: 0,
  unlisten: null,
  samples: null,   // [{t, b}] byte-total snapshots for the ETA's sliding-window rate
  etaText: "",     // last rendered ETA (repainted at most once a second)
  etaAt: 0,
};

// ---- ETA ----
// Rate over the last ~30 s of ticks, NOT the whole run: a resumed run starts with a byte jump,
// and the small-file stretches are request-bound — a whole-run average would haunt the estimate
// long after conditions changed. Repainted at most once a second: an ETA that flickers with
// every tick reads as noise. Says nothing for the first seconds — early guesses are wrong in
// both directions, and a number that appears and stabilizes beats one that thrashes.
function gdEta(now) {
  const s = gd.samples;
  s.push({ t: now, b: Math.min(gd.sum, gd.bytes) });
  while (s.length > 2 && now - s[0].t > 30000) s.shift();
  const span = now - s[0].t;
  if (span < 3000) return gd.etaText;
  // etaAt 0 = never painted -> paint now; afterwards at most once a second
  if (!gd.etaAt || now - gd.etaAt >= 1000) {
    const rate = (s[s.length - 1].b - s[0].b) / (span / 1000);
    if (rate > 0) {
      gd.etaAt = now;
      gd.etaText = t("gd.eta", { t: fmtDuration((gd.bytes - s[s.length - 1].b) / rate) });
    }
  }
  return gd.etaText;
}

// Coarse on purpose (nearest unit, minutes rounded): "~14 min left" is a promise the link can
// keep; "13:47" is one it can't. Minutes computed first so 59.9 min carries into "1 h 0 min"
// instead of rendering as "60 min".
function fmtDuration(sec) {
  const m = Math.round(sec / 60);
  if (m >= 60) return t("time.hm", { h: Math.floor(m / 60), m: m % 60 });
  if (m >= 1) return t("time.m", { m });
  return t("time.s", { s: Math.max(1, Math.round(sec)) });
}

function gdStage(stage) {
  for (const s of ["gd-dest", "gd-plan", "gd-confirm", "gd-run", "gd-err"])
    $(s).classList.toggle("hidden", s !== "gd-" + stage);
}

function gdClose() {
  // Closing during the PLAN stage has to stop the plan: it hashes an existing install for minutes,
  // and it used to keep reading the disk flat out after the dialog was gone, for a number nobody
  // would ever see. The run stage cancels through its own Stop button instead, and the closes that
  // follow a finished op find this stage hidden — so nothing else is touched.
  if (!$("gd-plan").classList.contains("hidden")) invoke("game_cancel").catch(() => {});
  $("gd-modal").classList.add("hidden");
  if (gd.unlisten) { gd.unlisten(); gd.unlisten = null; }
}

// Fresh download: pick a place, decide the DESTINATION, plan against it, confirm the numbers, run.
// The picker always runs — the download never silently reuses the configured game folder — and what
// it returns is a place, not yet the destination: the stage below composes one inside it (or not),
// and every screen after this names the composed path exactly. `game_install` then adopts that
// folder as the game dir backend-side, so a fresh download ends pointed at itself.
async function startGameDownload(origin) {
  if (state.busy) return;
  let dir = null;
  try {
    dir = await invoke("browse_folder", { title: t("dlg.pickTarget") });
  } catch (e) { /* dialog failed — stay put */ }
  if (!dir) return;
  gdDestOpen(origin, dir);
}

// ---- where the game goes ----
// A folder picker answers "which place", not "which folder": dropping ~15 GB of game plus the
// launcher's own bookkeeping into whatever was picked is a decision, and it used to be made
// silently. This stage makes it visible and reversible — a subfolder by default, renameable, and
// switchable off for people who picked an empty folder on purpose and want it used as-is.
//
// Nothing here touches the disk. The destination is created by the download itself, so backing out
// leaves the folder exactly as it was found.
const gdDest = {
  origin: null,  // where a finished download should land the user (gdOpen's own `origin`)
  base: null,    // what the picker returned — never edited on this screen
  name: "",      // the subfolder: the one editable piece
  nest: true,
  path: null,    // the composed destination, AS THE BACKEND COMPOSED IT — never joined here
  seq: 0,        // one keystroke can outrun another's round trip; only the newest answer renders
};

// Open on the picked folder. The default name and the "is something already here" facts both come
// from the backend, which is also what makes the opening state right in the one case that matters:
// a folder that ALREADY holds a game opens with the subfolder switched off, because nesting inside
// one installs a second copy a level down instead of continuing the install that is there.
async function gdDestOpen(origin, base) {
  gdDest.origin = origin;
  gdDest.base = base;
  gdDest.path = null;
  gdDest.seq++; // discard any refresh still in flight from a previous open
  gd.mode = "install";
  $("gd-title").textContent = t("gd.title");
  let v;
  try {
    // sub:null first — this call is asking what the default IS, and what the picked folder holds
    const probe = await invoke("game_target", { base, sub: null });
    gdDest.name = probe.defaultName;
    gdDest.nest = !probe.baseOccupied;
    $("gd-path-name").value = gdDest.name;
    v = gdDest.nest ? await invoke("game_target", { base, sub: gdDest.name }) : probe;
  } catch (e) {
    // A pure local read that cannot fail in a shipped build (no network, no op slot) — but if the
    // command is unreachable, say so instead of quietly downloading into the picked folder, which
    // is a different destination than this screen would have offered.
    $("gd-err-msg").textContent = errText(e);
    $("btn-gd-retry").classList.add("hidden");
    $("gd-modal").classList.remove("hidden");
    gdStage("err");
    return;
  }
  gdDestRender(v);
  $("gd-modal").classList.remove("hidden");
  gdStage("dest");
  // Take focus before syncModalLayer's observer does: its "first button in the card" is the switch
  // plate, and Enter on a dialog should mean its primary, not "toggle the option".
  $("btn-gd-dest-go").focus();
}

// Re-resolve from what is on screen. Runs on every keystroke: the backend composes the path and
// vets the name, so this screen never has to know what Windows allows or where a separator goes.
async function gdDestRefresh() {
  const seq = ++gdDest.seq;
  let v;
  try {
    v = await invoke("game_target", { base: gdDest.base, sub: gdDest.nest ? gdDest.name : null });
  } catch (e) {
    return; // a stale line beats a half-rendered one; the next keystroke re-asks
  }
  // a newer keystroke has already been answered, or the screen is gone (closed, or moved on to the
  // plan — a late answer must not write into either)
  const onDest = !$("gd-modal").classList.contains("hidden") &&
                 !$("gd-dest").classList.contains("hidden");
  if (seq !== gdDest.seq || !onDest) return;
  gdDestRender(v);
}

function gdDestRender(v) {
  gdDest.path = v.path;
  // The separator is SPLIT OUT of the backend's prefix, never invented here — it has to render in
  // its own left-to-right span (see .gd-path-base). Relocating the character the backend produced
  // is not the same as joining a path frontend-side, which is the thing that must not happen.
  const sep = /[\\/]$/.test(v.prefix) ? v.prefix.slice(-1) : "";
  $("gd-path-base").textContent = sep ? v.prefix.slice(0, -1) : v.prefix;
  $("gd-path-sep").textContent = sep;
  // the whole path on hover, for when the head is ellipsized. Nothing to show when the name is
  // refused: there is no such path, and pasting one together here to fill the gap is exactly the
  // frontend join this screen is built to avoid.
  $("gd-path").title = v.path || "";
  // the refused text wears the refusal: the message below says why, the field says which characters
  $("gd-path").classList.toggle("bad", !!v.nameError);
  $("gd-path-name").classList.toggle("hidden", !gdDest.nest);
  $("gd-nest").classList.toggle("on", gdDest.nest);
  $("gd-nest").setAttribute("aria-checked", String(gdDest.nest));
  $("gd-nest").querySelector(".switch").classList.toggle("on", gdDest.nest);

  // The note is composed, not picked: several of these facts can be true at once, and each is one
  // line. Order is by weight — what this run would do to an install that already exists first, what
  // the folder will look like afterwards last. No two lines may state the same consequence: the
  // count and the plain form of the foreign-files warning are the same sentence, so it is one line
  // either way.
  const lines = [];
  let bad = false;
  if (v.nameError) {
    lines.push(t("gd.name." + v.nameError, { name: gdDest.name }));
    bad = true;
  } else if (v.occupied) {
    // there is already a game (or half of one) at the destination — the shape of the folder is
    // settled, and what matters is that this continues it
    if (gdDest.nest && v.baseOccupied) lines.push(t("gd.destNestInGame"));
    lines.push(t("gd.destBusy"));
  } else {
    if (gdDest.nest && v.baseOccupied) lines.push(t("gd.destNestInGame"));
    if (!gdDest.nest) lines.push(t("gd.destFlat"));
    // stated whenever the folder is shared — because it already holds things, or because it is
    // about to (switching the subfolder off is exactly that)
    if (!gdDest.nest || v.foreignEntries > 0) {
      lines.push(v.foreignEntries > 0
        ? t("gd.destForeignN", { n: v.foreignEntries })
        : t("gd.destForeign"));
    }
  }
  const note = $("gd-dest-note");
  // nothing to warn about: the quiet hint, unplated
  note.classList.toggle("notice", lines.length > 0);
  note.classList.toggle("err", bad);
  const text = lines.length ? lines.join("\n") : t("gd.destNestHint");
  const frag = inlineCode(text);
  if (frag) note.replaceChildren(frag);
  else note.textContent = text;
  // a destination that cannot exist must not be sendable
  $("btn-gd-dest-go").disabled = !v.path;
}

// Resume/continue into the CONFIGURED folder — the one entry point that reuses it instead of
// asking, because here "where" was already answered: the folder is configured AND (usually)
// holds the interrupted download's cache. The confirm still names the exact path and now says
// how much is already fetched, so no byte moves on an assumption.
function startGameResume() {
  if (state.busy) return;
  const dir = state.lastCheck && state.lastCheck.gameDir;
  if (dir) gdOpen(null, dir);
}

// Plan + confirm, against a destination that is already decided: `dir` is the composed path the
// destination stage produced (or the configured folder, when resuming). Nothing is joined to it
// here — every line on the confirm names this exact string, and so does `game_install`.
async function gdOpen(origin, dir) {
  gd.mode = "install";
  gd.origin = origin;
  gd.dir = dir;
  $("gd-title").textContent = t("gd.title");
  $("gd-modal").classList.remove("hidden");
  gdStage("plan");
  $("gd-plan-line").textContent = t("gd.planning");
  $("gd-plan-sub").textContent = "";
  let plan;
  // planning an empty folder is instant, but a folder that already holds a game gets hashed —
  // minutes of work that must not look like a hung spinner
  let planUnlisten = null;
  try {
    planUnlisten = await listen("op-progress", (ev) => {
      const p = ev.payload;
      if (p.op !== "plan") return;
      $("gd-plan-line").textContent = t("gd.checking", { i: p.current, n: p.total });
      $("gd-plan-sub").textContent = p.item || "";
    });
    plan = await invoke("game_plan", { target: dir });
  } catch (e) {
    if (e && e.kind === "cancelled") return;                // gdClose asked for it — no error theater
    if ($("gd-modal").classList.contains("hidden")) return; // Escaped while planning
    $("gd-err-msg").textContent = errText(e);
    $("btn-gd-retry").classList.add("hidden"); // nothing started — reopening re-plans anyway
    gdStage("err");
    return;
  } finally {
    // released on every path, including the early returns above — otherwise each reopen of the
    // dialog would stack another live listener
    if (planUnlisten) planUnlisten();
  }
  if ($("gd-modal").classList.contains("hidden")) return;
  // WIRE bytes: bundles compress, so what downloads and what lands on disk are two different
  // numbers now — the bar/ETA/"downloaded so far" all speak the wire, the confirm names both
  gd.bytes = plan.bytes;
  gd.files = plan.files;
  const disk = (plan.diskBytes / GB).toFixed(1);
  // an interrupted attempt left verified-size entries/.parts in the cache: say how much of the
  // plan is already here — bytes AND files ("did my 5 GB survive the restart?"). The files
  // number is completely-fetched only; a .part counts toward the bytes, not the files.
  $("gd-summary").textContent = plan.cachedBytes > 0
    ? t("gd.confirmResume", {
        have: (plan.cachedBytes / GB).toFixed(1), gb: (plan.bytes / GB).toFixed(1),
        df: plan.cachedFiles, n: plan.files, disk, dir,
      })
    : t("gd.confirm", { gb: (plan.bytes / GB).toFixed(1), n: plan.files, disk, dir });
  // refuse up front what the backend would refuse a click later — its exact demand (decoded
  // footprint + packed-bundle transient, computed backend-side) plus the same 512 MB margin
  const short = plan.freeBytes != null && plan.freeBytes < plan.needBytes + 512 * 1024 * 1024;
  $("gd-space").hidden = !short;
  if (short) {
    $("gd-space").textContent = t("gd.noSpace", {
      need: (plan.needBytes / GB).toFixed(1), free: (plan.freeBytes / GB).toFixed(1),
    });
  }
  $("btn-gd-go").disabled = short;
  gdStage("confirm");
}

// Repair: the files view already confirmed — straight to the run stage.
//
// `restore` and `keep` are the two halves of the user's decision, and both travel: the backend
// records the pins BEFORE it downloads anything, so a failure mid-fetch cannot lose the "leave
// these alone" half and re-offer their mods for overwrite on the retry.
async function startGameRepair(v, restore, keep, thenPhoenix) {
  gd.mode = "repair";
  gd.origin = null;
  gd.restore = restore;
  gd.keep = keep;
  // A selection can span both authorities (a mod that replaced a vanilla file AND a Phoenix one).
  // They are two repos and two pipelines, so they run in sequence — the game part owns this modal,
  // then hands over. Dropped on failure or cancel on purpose: the second half is not something to
  // start while the first is in an unknown state.
  gd.thenPhoenix = thenPhoenix || null;
  // the selection's own wire cost, by DISTINCT asset: two files in one bundle are one download,
  // which is exactly how the backend totals it (see gvTally)
  const keys = new Map();
  for (const p of restore) {
    const f = v.files.find((x) => x.path === p);
    if (f && f.wireKey) keys.set(f.wireKey, f.wire);
  }
  gd.bytes = [...keys.values()].reduce((a, b) => a + b, 0);
  gd.files = restore.length;
  $("gd-title").textContent = t("gd.repairTitle");
  $("gd-modal").classList.remove("hidden");
  gdRun();
}

function onGdProgress(ev) {
  const p = ev.payload;
  if (p.op === "game") {
    // The tick SAYS which half of the run it belongs to. It used to be inferred from "does it
    // carry bytes", which reads a plan tick for a big file (the hash is narrated too) as a
    // download — see engine::OpProgress::phase for what that cost.
    if (p.phase === "plan") {
      // Once the download has started this is a tick from a phase that is OVER: `emit` hands
      // events to the webview asynchronously, so the tail of the plan's ticks is still draining
      // while the first files download, and each one repainted knocked the download line back to
      // "Checking existing files…". One-way within a run; gdRun resets it.
      if (gd.phase === "fetch") return;
      $("gd-line1").textContent = t("gd.checking", { i: p.current, n: p.total });
      $("gd-line2").textContent = p.item || "";
    } else {
      gd.phase = "fetch";
      // running total, updated by DELTA. Re-summing the map per tick was O(files) inside an
      // O(ticks) stream — ~60k ticks against a map growing to 4,635 entries is a few hundred
      // million iterations of pure waste on the UI thread during the download.
      //
      // The delta keeps `sum` EXACTLY equal to the map's values summed, and every value is a byte
      // count — which is what makes the line safe against a retry that restarts BELOW its own
      // high-water mark (the backend's `abs_diff` case: a poisoned .part discarded, a server
      // declining the Range). Such a tick walks the total down by that item's own contribution
      // and no further, so it can report less than it did a second ago but never less than zero.
      // Do NOT "fix" that with Math.max(0, …): the clamp breaks the equality, and the next tick
      // for a DIFFERENT item then subtracts an old value the total no longer holds — inventing
      // the negative it was added to prevent.
      gd.sum += p.bytesDone - (gd.perFile.get(p.item) || 0);
      gd.perFile.set(p.item, p.bytesDone);
      if (p.done) gd.doneFiles = Math.min(gd.files, gd.doneFiles + 1);
      const sum = Math.min(gd.sum, gd.bytes); // shared-content dests double-tick; the bar must not
      const pct = gd.bytes ? (sum / gd.bytes) * 100 : 100;
      $("gd-fill").style.width = pct.toFixed(1) + "%";
      const eta = gdEta(performance.now());
      $("gd-line1").textContent = t("gd.dl", {
        done: (sum / GB).toFixed(2), total: (gd.bytes / GB).toFixed(2),
        i: gd.doneFiles, n: gd.files,
      }) + (eta ? " · " + eta : "");
      $("gd-line2").textContent = p.item || "";
    }
  } else if (p.op === "install") {
    // the chained shim install after a fresh download — small, so one settled bar + a line
    $("gd-fill").style.width = "100%";
    $("gd-line1").textContent = t("gd.shim");
    $("gd-line2").textContent = p.item || "";
  }
}

async function gdRun() {
  // guarded like every other mutating flow — this one had no check at all, so a second entry
  // (the modal's Resume button, a re-fired click) could run two game installs at once
  const busy = acquireBusy();
  if (busy == null) return;
  gdStage("run");
  gd.perFile = new Map();
  gd.sum = 0;      // running byte total; reset with the map or a resumed run inherits it
  gd.phase = "plan"; // "plan" -> "fetch", one way, per run — see onGdProgress
  gd.doneFiles = 0;
  gd.samples = []; // fresh rate window — a retry must not inherit the failed run's rate
  gd.etaText = "";
  gd.etaAt = 0;
  $("gd-fill").style.width = "0%";
  $("gd-line1").textContent = t("gd.starting");
  $("gd-line2").textContent = "";
  if (gd.unlisten) { gd.unlisten(); gd.unlisten = null; }
  gd.unlisten = await listen("op-progress", onGdProgress);
  let result = null;
  try {
    result = gd.mode === "repair"
      ? await invoke("game_repair", { restore: gd.restore, keep: gd.keep })
      : await invoke("game_install", { target: gd.dir });
  } catch (e) {
    releaseBusy(busy);
    if (e && e.kind === "cancelled") {
      gdClose(); // the user asked for the stop — no error theater; reopening resumes
      return;
    }
    if (gd.unlisten) { gd.unlisten(); gd.unlisten = null; }
    $("gd-err-msg").textContent = errText(e);
    // a mid-download failure is resumable by construction — offer that instead of a dead end
    $("btn-gd-retry").classList.remove("hidden");
    gdStage("err");
    return;
  }
  releaseBusy(busy);
  gdClose();
  if (gd.mode === "repair") {
    // Only what was actually PUT BACK clears. This used to zero unconditionally, so the two
    // commonest partial gestures unblocked Play against files nobody repaired: restoring 1 of 5
    // missing files, and — worse — "Select none" then the primary, which is a keeps-only run that
    // downloads nothing at all and returned `written: 0`. Play was then offered over a folder this
    // app had declared broken seconds earlier. Counting down by what landed keeps the block
    // proportional; a fresh verify is what clears the rest.
    state.gameDamaged = Math.max(0, state.gameDamaged - result.written);
    renderPrimary();
    // and the verdict word follows the same fact: nothing restored is not "intact"
    if (result.written > 0) {
      setStatus(t("status.gvOk"), "ok", t("gv.repaired", { n: result.written }));
    } else if (gd.keep.length) {
      setStatus(t("status.yourFiles"), "ok", t("gv.keptOnly", { n: gd.keep.length }));
    } else {
      setStatus(t("status.gvOk"), "ok", t("gv.repaired", { n: 0 }));
    }
    // The files view is still underneath, and every row on it now describes a folder that has
    // changed. Land on main, where the verdict it just produced is written.
    if (currentView() === "gv") {
      showView("main");
      $("view-main").classList.add("revealed");
      doCheck();
    }
    // the Phoenix half of the same selection, if there was one — see startGameRepair
    if (gd.thenPhoenix && gd.thenPhoenix.length) {
      const phx = gd.thenPhoenix;
      gd.thenPhoenix = null;
      doApply(phx);
    }
    return;
  }
  // fresh install: the folder is the game dir now (backend adopted it before the shim chain) —
  // and a damaged-verdict from some OTHER folder must not keep blocking Play against bytes that
  // were just written pristine
  state.gameDamaged = 0;
  if (gd.origin === "settings") {
    // keep the settings form honest — a later Save must not overwrite the new game dir with
    // the stale input value
    $("in-game").value = gd.dir;
    if (state.settingsLoaded) state.settingsLoaded.game = gd.dir;
    // the snapshot is a STRING (settingsSnapshot serializes) — re-take it, don't try to poke a
    // field into it, or the folder we just wrote would read as an unsaved edit
    if (state.settingsSnap !== null) state.settingsSnap = settingsSnapshot();
  }
  showView("main");
  $("view-main").classList.add("revealed");
  doCheck();
}

// ---- verify game files ----
// Runs inside its own modal on top of whatever view asked for it. It used to leave for the main
// view (because that is where the status line lives), report there, and then open a third screen
// when it finished — one action that looked like three, and that threw the user out of Settings
// where they had pressed the button. The main status line is still written, because it is the
// right place for the verdict once the user does go back; it is just no longer navigated to.
function vfStage(stage) {
  for (const s of ["vf-run", "vf-done"]) $(s).classList.toggle("hidden", s !== stage);
}

// "What in this folder is MINE" — the pins plus everything nothing claims, with no integrity pass
// behind it. It shares the verify modal and the files view; what it does NOT share is the cost.
// Reaching a kept file used to mean sitting through a full verification (minutes of hashing) to
// get a list whose interesting rows were already known from the keep file.
async function doYourFiles(origin) {
  const busy = acquireBusy();
  if (busy == null) return;
  origin = origin || currentView();
  $("vf-title").textContent = t("yf.title");
  $("vf-line1").textContent = t("yf.reading");
  $("vf-line2").textContent = "";
  $("btn-vf-stop").disabled = false;
  vfStage("vf-run");
  $("vf-modal").classList.remove("hidden");
  // The extras walk and the pin hashing both poll the shared cancel flag, so Stop means it here
  // too — a folder with a million files in it can still take a while to walk.
  $("btn-vf-stop").onclick = () => {
    $("btn-vf-stop").disabled = true;
    $("vf-line1").textContent = t("gv.stopping");
    invoke("game_cancel").catch(() => {});
  };
  let result = null;
  let done = null;
  try {
    const r = await invoke("your_files");
    if (r.files.length) result = r;
    else done = t("gv.yoursNone"); // an empty view would make the user wonder what failed
  } catch (e) {
    if (e && e.kind === "cancelled") done = t("gv.stopped");
    else { onError(e); done = errText(e); }
  } finally {
    $("btn-vf-stop").onclick = null;
    releaseBusy(busy); // before the view opens — its footer greys itself out while an op holds
  }
  if (result) {
    $("vf-modal").classList.add("hidden");
    openFilesView(result, origin, "yours");
  } else {
    $("vf-result").textContent = done || "";
    vfStage("vf-done");
  }
}

async function doGameVerify(origin) {
  const busy = acquireBusy();
  if (busy == null) return;
  origin = origin || currentView();
  setStatus(t("status.working"), "busy", t("gv.start"));
  $("vf-title").textContent = t("vf.title");
  $("vf-line1").textContent = t("gv.start");
  $("vf-line2").textContent = "";
  $("btn-vf-stop").disabled = false;
  vfStage("vf-run");
  $("vf-modal").classList.remove("hidden");

  let unlisten = null;
  let stopping = false;
  $("btn-vf-stop").onclick = () => {
    // the hash workers stop BETWEEN files, so a multi-GB VPK in flight still has to finish; say
    // that rather than leaving a dead button over an unchanged line
    stopping = true;
    $("btn-vf-stop").disabled = true;
    $("vf-line1").textContent = t("gv.stopping");
    $("vf-line2").textContent = "";
    invoke("game_cancel").catch(() => {});
  };

  let damagedResult = null;
  let viewResult = null;
  let done = null; // a verdict with no list behind it — shown in the modal instead
  try {
    unlisten = await listen("op-progress", (ev) => {
      const p = ev.payload;
      if (p.op !== "verify" || stopping) return; // ticks in flight must not repaint "Stopping…"
      $("vf-line1").textContent = t("gv.progress", { i: p.current, n: p.total });
      // Big files carry byte ticks. Without them the counter simply stops for tens of seconds on
      // each multi-hundred-MB VPK while the CPU is pegged, which is indistinguishable from a hang.
      $("vf-line2").textContent = p.bytesTotal
        ? t("gv.progressBytes", {
            item: p.item || "",
            done: fmtBytes(p.bytesDone),
            total: fmtBytes(p.bytesTotal),
          })
        : p.item || "";
    });
    const r = await invoke("game_verify");
    // Counted by KIND, because the states mean different things and one number for all of them
    // would be the overclaim this whole split exists to remove: "missing" is unambiguous,
    // "modified" might be a mod, "unreadable" is a verdict nobody could reach, and "kept" is the
    // user's own earlier decision, not a problem at all.
    const n = { missing: 0, modified: 0, unreadable: 0, kept: 0, extra: 0 };
    for (const f of r.files) n[f.state === "extraDir" ? "extra" : f.state]++;
    const actionable = n.missing + n.modified + n.unreadable;
    if (actionable === 0) {
      // its own word, NOT "Up to date": that one describes the Phoenix files, and the list on main
      // still shows whatever the shim's own state is. Two different subjects were sharing one
      // status line, so "Up to date" could sit above "2 to change".
      state.gameDamaged = 0; // the files are provably fine — clear any earlier declined repair
      setStatus(t("status.gvOk"), "ok", t("gv.ok", { n: r.ok, version: r.version }));
      done = t("gv.ok", { n: r.ok, version: r.version });
    } else if (r.foreignBuild) {
      // NOT damage: this folder holds a different build, so a "repair" would overwrite a working
      // unrelated install. Repairing is still allowed — it is the user's folder, and the project
      // does not gate on build elsewhere either — but it is named for what it is, in the error
      // colour, and the files view's confirm spells out the consequence.
      // This verdict SUPERSEDES an older damaged one: the verify just concluded nothing here is
      // broken, so a damaged count from a previous run must not keep blocking Play.
      state.gameDamaged = 0;
      setStatus(t("status.gvForeign"), "error", t("gv.foreign", { version: r.version }));
      damagedResult = r;
    } else if (n.unreadable > 0) {
      // Leads over the other counts: unreadable files are the unusual condition and the only one
      // whose fix lives outside this app, so naming them is what makes the line actionable. Error
      // colour, not the update gold — nothing here is an update.
      setStatus(
        t("status.gvUnreadable"),
        "error",
        t("gv.unreadable", { n: n.unreadable, d: n.missing + n.modified })
      );
      damagedResult = r;
    } else if (n.missing > 0) {
      // Missing is the unambiguous half: a file that is absent is absent, whoever owns it.
      setStatus(t("status.gvDamaged"), "update", t("gv.damaged", { n: n.missing, d: n.modified }));
      damagedResult = r;
    } else {
      // Only DIFFERENCES. `BaseAction::Differs` exists precisely to say "different, not damaged" —
      // and the commonest cause is a mod the user installed on purpose. Leading with "Game files
      // damaged" over one edited file contradicts the screen it opens, which lists that file as a
      // choice to make. It is also why no ratio is printed any more: `total` counts the BASE plan
      // alone, while these counts include the Phoenix half, so "1 of 4635" compared a numerator
      // and a denominator drawn from different sets.
      setStatus(t("status.gvDiffer"), "update", t("gv.differ", { n: n.modified }));
      damagedResult = r;
    }
    // Even a clean verify has something to LOOK at when kept files or extras exist — those are
    // not problems, but "what is in my game folder that isn't the game" is exactly the question
    // this screen answers, and answering it only on failure would be perverse. Opened AFTER the
    // busy latch is released, below: the view's footer disables its buttons while an op owns the
    // UI, and rendering it from in here left them dead with nothing left to re-enable them.
    if (r.files.length) viewResult = r;
    else done = done || t("gv.ok", { n: r.ok, version: r.version });
  } catch (e) {
    // The user asked for this stop, so it is not an error — but it is not a verdict either. The
    // neutral wording keeps it from reading as "checked, all fine": the files nobody got to are
    // exactly the ones this run can say nothing about. An earlier declined repair still stands
    // (`state.gameDamaged` is untouched), so Play stays blocked if it was.
    if (e && e.kind === "cancelled") {
      setStatus(t("status.gvStopped"), "idle", t("gv.stopped"));
      done = t("gv.stopped");
    } else {
      onError(e);
      done = errText(e);
    }
  } finally {
    // unlisten BEFORE releasing: a tick emitted just before the workers joined can still be in
    // flight, and it would repaint the finished modal with a dead progress line.
    if (unlisten) unlisten();
    $("btn-vf-stop").onclick = null;
    releaseBusy(busy);
  }

  if (damagedResult && !damagedResult.foreignBuild) {
    // Leaving the files view without restoring leaves the game files as they were. Remember that:
    // Play must not go on offering to launch a client this very check reported as broken.
    //
    // MISSING only, deliberately. A file that is simply absent is the one verdict with no second
    // reading; "modified" may well be a mod the user installed on purpose, and blocking Play on
    // it would be exactly the overclaim this whole screen exists to remove. Unreadable is not a
    // statement about content at all.
    state.gameDamaged = damagedResult.files.filter((f) => f.state === "missing").length;
    renderPrimary();
  }
  // A FOREIGN build is different again: verify's own verdict is that nothing is damaged — the
  // folder holds another build, and the whole point of the distinction is that it works. Blocking
  // Play here painted a working install as broken with no way out short of overwriting it. No
  // install gate: the status line keeps saying "different build", and launching stays the user's
  // call.

  if (viewResult) {
    $("vf-modal").classList.add("hidden");
    openFilesView(viewResult, origin);
  } else {
    $("vf-result").textContent = done || "";
    vfStage("vf-done");
  }
}

// ---- files view: what a verify found, and what to do about it ----
//
// The problem this screen solves is scale. A verify over a modded install comes back with tens of
// differences; over a foreign build, with four and a half thousand. A flat list is useless at the
// second number and only barely tolerable at the first, so the spine is the FOLDER TREE: mods land
// in identifiable subtrees, which means 312 differences under panorama/ collapse to one row that
// can be spared with one click. Three mechanisms stack on top of it, cheapest first — facet chips
// (a whole kind of difference in or out), a path filter, and per-folder tri-state selection.
//
// ONE selection model: every row has a checkbox, and what checking it means is decided by who
// owns the row — put this file back (game/Phoenix), or delete it (nothing owns it). A checked
// extra turns terracotta and struck through, so the difference is visible on the row itself and
// the footer names both acts separately. This replaced a second, near-invisible per-row control
// for deletion, which had a much worse failure than being ugly: a user whose results were ALL
// extras saw bulk controls that skipped every row on screen and a primary button stuck at
// "Keep 0 as they are". The safety that control was protecting now comes from where it belongs —
// extras are hidden until their chip is turned on, deletion is its own terracotta button, and its
// confirm names the count and says it cannot be undone.
//
// Unchecking a `modified` row is not passive: it is the user saying "this one is mine", and it is
// persisted as a content pin so the next verify stops asking. The footer states that count beside
// the restore count, and the confirm repeats both — a decision this durable is never inferred
// silently.
const gv = {
  data: null,      // the GameVerifyView this screen is describing
  root: null,      // the folder trie
  sel: new Set(),  // every checked path — see gvTally for what checking one MEANS
  mode: "verify",  // "verify" = what a scan found | "update" = which of MY files a release may overwrite
  origin: "main",  // the view Back returns to; Verify is launched from settings as often as not
  facets: new Set(),
  filter: "",
  warn: "",       // standing warnings (foreign build, unreachable shim, truncated scan)
  flat: [],        // the rows currently visible, in order — what the virtual window indexes into
  pool: [],        // recycled row elements; scrolling rebinds these instead of building DOM
  rowH: 34,   // replaced by a real measurement the first time the view is shown
  working: false, // an act is in flight: buttons disabled, rows inert (see gvWorking)
  cursor: 0,  // the keyboard's row, as an index into `flat` — the ELEMENTS are a recycled pool,
              // so nothing durable can hold the focus but the index (see gvFocus)
};

// Which chip governs a row. `extraDir` rides with `extra`: a summarized subtree is one of them,
// just counted rather than listed.
const gvKind = (f) => (f.state === "extraDir" ? "extra" : f.state);
// A row whose checkbox means something. Extras have nothing to be restored TO.
const gvRestorable = (f) => f.owner !== "extra";

// The chip order is the order of alarm, not of frequency: what is missing or unreadable is a
// problem, what is modified or kept is a choice, and extras are context.
const GV_KINDS = ["missing", "unreadable", "modified", "kept", "extra"];
const GV_STATE_KEY = {
  missing: "gv.state.missing",
  modified: "gv.state.modified",
  unreadable: "gv.state.unreadable",
  kept: "gv.state.kept",
  extra: "gv.state.extra",
  extraDir: "gv.state.extraDir",
};

function fmtBytes(n) {
  if (n == null) return "";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i === 0 ? n : n.toFixed(n < 10 ? 1 : 0)) + " " + u[i];
}

// Coarse on purpose. The question this answers is "was this touched around when the game was
// installed, or long after?", and a day is plenty of resolution for it.
function fmtAge(mtime) {
  if (!mtime) return "";
  const days = Math.floor((Date.now() / 1000 - mtime) / 86400);
  if (days < 1) return t("gv.age.today");
  if (days < 30) return t("gv.age.d", { n: days });
  if (days < 365) return t("gv.age.mo", { n: Math.floor(days / 30) });
  return t("gv.age.y", { n: Math.floor(days / 365) });
}

// ---- the tree ----
// Built once per verify. Structure never depends on the filter or the facets: a tree that
// reshapes itself as you type would move the row under the cursor between the click and the
// mouseup.
function gvBuild(files) {
  const root = { name: "", path: "", dir: true, kids: new Map(), files: [], open: true };
  for (const f of files) {
    f.lc = f.path.toLowerCase(); // the filter compares against this on every rebuild
    const parts = f.path.split("/");
    let n = root;
    for (let i = 0; i < parts.length - 1; i++) {
      const seg = parts[i];
      let k = n.kids.get(seg);
      if (!k) {
        k = {
          name: seg,
          path: n.path ? n.path + "/" + seg : seg,
          dir: true, kids: new Map(), files: [], open: false,
        };
        n.kids.set(seg, k);
      }
      n = k;
    }
    // an `extraDir` row IS a directory in the folder, but it is a LEAF here: the whole point of
    // summarizing it was not to walk into it
    n.files.push(f);
  }
  gvCompress(root);
  gvSort(root);
  return root;
}

// Order is a property of the TREE, not of the current filter, so it is settled once. Sorting
// inside the flatten meant re-running localeCompare over every level on every keystroke — tens of
// thousands of collator calls per character typed into the filter box.
function gvSort(n) {
  n.order = [...n.kids.values()].sort((a, b) => a.name.localeCompare(b.name));
  n.files.sort((a, b) => a.path.localeCompare(b.path));
  for (const k of n.order) gvSort(k);
}

// Collapse chains of single-child directories into one row (`game/dota/panorama/layout` instead
// of four rows each holding only the next). Without this the tree is mostly corridors: a user
// clicks four times to reach anything, and three quarters of a screenful says nothing.
function gvCompress(n) {
  for (const k of n.kids.values()) gvCompress(k);
  if (n.path && n.kids.size === 1 && n.files.length === 0) {
    const [only] = n.kids.values();
    n.name += "/" + only.name;
    n.path = only.path;
    n.kids = only.kids;
    n.files = only.files;
  }
}

// ---- visibility + aggregates, one pass ----
// Every directory row states how many rows it holds and how many are picked, and those numbers
// must agree with what the chips and the filter are currently showing — otherwise a folder reads
// "312" while displaying four. So the counts are computed over VISIBLE descendants, here, on the
// same walk that decides visibility.
function gvCompute(n) {
  const a = { vis: 0, sel: 0, extras: 0 };
  for (const k of n.kids.values()) {
    const ka = gvCompute(k);
    a.vis += ka.vis; a.sel += ka.sel; a.extras += ka.extras;
  }
  n.visFiles = n.files.filter(gvPass);
  for (const f of n.visFiles) {
    a.vis++;
    if (gv.sel.has(f.path)) a.sel++;
    if (!gvRestorable(f)) a.extras++;
  }
  n.agg = a;
  return a;
}

function gvPass(f) {
  if (!gv.facets.has(gvKind(f))) return false;
  if (gv.filter && !f.lc.includes(gv.filter)) return false;
  return true;
}

// Flatten the open parts of the tree into the row array the virtual window indexes. Directories
// before files, both alphabetical — predictable beats clever when someone is hunting for a path.
function gvFlatten(n, depth, out) {
  for (const k of n.order) {
    if (k.agg.vis === 0) continue;
    out.push({ node: k, depth });
    // A filter is a search, and a search that makes you open folders to see its hits is not one.
    if (k.open || gv.filter) gvFlatten(k, depth + 1, out);
  }
  // visFiles is a filter of the pre-sorted files, and filter preserves order
  for (const f of n.visFiles) out.push({ file: f, depth });
}

// What ticking a box actually does, in the words of THIS screen. Rebuilt with the tree because
// it depends on the mode and on whether the extras chip is on — a tick means "put this back" on
// one row and "delete this permanently" on the next, and a checkbox cannot say that by itself.
function renderGvLegend() {
  const parts = [];
  if (gv.data.files.some(gvRestorable)) {
    parts.push(t(gv.mode === "update" ? "gv.legendUpdate" : "gv.legendRestore"));
  }
  if (gv.facets.has("extra") && gv.data.files.some((f) => !gvRestorable(f))) {
    parts.push(t("gv.legendExtras"));
  }
  const el = $("gv-legend");
  el.textContent = "";
  // the verbs are the load-bearing words, so they are the ones that get the colour
  for (const [i, p] of parts.entries()) {
    if (i) el.append(" · ");
    for (const [j, chunk] of p.split("**").entries()) {
      if (j % 2) el.append(Object.assign(document.createElement("b"), { textContent: chunk }));
      else el.append(chunk);
    }
  }
}

function gvRebuild() {
  gvCompute(gv.root);
  gv.flat = [];
  gvFlatten(gv.root, 0, gv.flat);
  // collapsing a folder (or typing in the filter) can shorten the list under the keyboard's row
  gv.cursor = Math.max(0, Math.min(gv.cursor, gv.flat.length - 1));
  renderGv();
  renderGvLegend();
  renderGvFooter();
}

// ---- the virtual window ----
// Rows are absolutely positioned by index, so only the ~20 on screen exist. The pool is recycled
// rather than rebuilt: at 4,635 rows, recreating the DOM on every scroll tick is what turns a
// list into a slideshow.
function renderGv() {
  const area = $("gv-area"), list = $("gv-list");
  const H = gv.rowH;
  list.style.height = gv.flat.length * H + "px";
  const empty = gv.flat.length === 0;
  $("gv-empty").classList.toggle("hidden", !empty);
  // "nothing matches this filter" is a lie when there is nothing left to match — which is exactly
  // the state an extras-only view lands in after its rows are deleted
  $("gv-empty").textContent = empty
    ? (gv.data.files.length ? t("gv.emptyFiltered") : t("gv.emptyAll"))
    : "";

  const first = Math.max(0, Math.floor(area.scrollTop / H) - 6);
  const last = Math.min(gv.flat.length, Math.ceil((area.scrollTop + area.clientHeight) / H) + 6);
  const need = Math.max(0, last - first);
  while (gv.pool.length < need) {
    const el = gvRowEl();
    list.append(el);
    gv.pool.push(el);
  }
  for (let i = 0; i < gv.pool.length; i++) {
    const el = gv.pool[i];
    const idx = first + i;
    if (i >= need || idx >= gv.flat.length) { el.classList.add("hidden"); continue; }
    el.classList.remove("hidden");
    el.style.top = idx * H + "px";
    gvFill(el, gv.flat[idx], idx);
  }
}

// One row's skeleton, built once and rebound forever after.
function gvRowEl() {
  const el = document.createElement("div");
  el.className = "gv-row";
  // Reachable and operable from the keyboard. This screen's entire purpose is a per-file decision,
  // and it used to expose none of them: rows were plain divs with a click listener, so the only
  // keyboard-reachable acts on it were "restore everything visible" and "restore nothing".
  // ONE tab stop for the whole list (roving focus, moved by the arrow keys) rather than 4,635 —
  // tabbing through a virtualised list is not navigation, and the pooled elements are recycled as
  // you scroll, so per-row tab stops would not survive their own rows anyway.
  el.tabIndex = -1;
  el.innerHTML =
    '<span class="gv-twist"><svg viewBox="0 0 12 12"><polyline points="4,2 8,6 4,10"/></svg></span>' +
    '<span class="gv-box"></span>' +
    '<span class="gv-name"></span>' +
    '<span class="gv-badge hidden"></span>' +
    '<span class="gv-meta"></span>';
  return el;
}

function gvFill(el, item, idx) {
  el.dataset.i = idx;
  // roving: one tab stop for the list, on whichever row the keyboard is currently on
  el.tabIndex = idx === gv.cursor ? 0 : -1;
  el.style.setProperty("--d", item.depth);
  const twist = el.children[0], box = el.children[1], name = el.children[2];
  const badge = el.children[3], meta = el.children[4];

  if (item.node) {
    const n = item.node, a = n.agg;
    el.classList.add("dir");
    el.classList.toggle("open", !!n.open || !!gv.filter);
    el.classList.remove("doomed");
    twist.classList.remove("leaf");
    twist.classList.remove("hidden");
    // a compressed chain greys everything but the folder this row actually is
    const cut = n.name.lastIndexOf("/");
    name.textContent = "";
    if (cut >= 0) {
      const lead = document.createElement("span");
      lead.className = "gv-lead";
      lead.textContent = n.name.slice(0, cut + 1);
      name.append(lead, n.name.slice(cut + 1));
    } else {
      name.textContent = n.name;
    }
    box.className = "gv-box";
    box.dataset.s = a.sel === 0 ? "off" : a.sel === a.vis ? "on" : "part";
    // what the row IS, for anything not reading pixels: a checkbox whose third state is real
    el.setAttribute("role", "checkbox");
    el.setAttribute("aria-checked", a.sel === 0 ? "false" : a.sel === a.vis ? "true" : "mixed");
    el.setAttribute("aria-expanded", String(!!n.open || !!gv.filter));
    el.setAttribute("aria-label", n.name);
    badge.classList.add("hidden");
    meta.className = "gv-count";
    meta.textContent = t("gv.dirCount", { n: a.vis, sel: a.sel });
    return;
  }

  const f = item.file;
  const picked = gv.sel.has(f.path);
  const restorable = gvRestorable(f);
  el.classList.remove("dir", "open");
  // a checked extra is about to be DELETED — the row says so itself
  el.classList.toggle("doomed", picked && !restorable);
  twist.classList.add("leaf");
  name.textContent = f.path.slice(f.path.lastIndexOf("/") + 1);
  box.className = "gv-box" + (restorable ? "" : " del");
  box.dataset.s = picked ? "on" : "off";
  el.setAttribute("role", "checkbox");
  el.setAttribute("aria-checked", String(picked));
  el.removeAttribute("aria-expanded");
  // the path plus what is wrong with it — a screen reader landing here should not have to hunt
  // for the state word sitting in a different span
  el.setAttribute("aria-label", f.path + ", " + t(GV_STATE_KEY[f.state] || "gv.state.modified"));
  // Named in full, not initialled. These rows are the ones a user is most likely to misread —
  // Phoenix's files sit among the game's and look no different — and "PHX" is a puzzle where
  // "PHOENIX" is an answer.
  badge.className = "gv-badge phx" + (f.owner === "phoenix" ? "" : " hidden");
  badge.textContent = "PHOENIX";

  meta.className = "gv-meta";
  meta.textContent = "";
  const st = document.createElement("span");
  st.className = "gv-state " + f.state;
  st.textContent = t(GV_STATE_KEY[f.state] || "gv.state.modified");
  meta.append(st);
  // Two facts, both true, shown as both: these are your bytes AND the release has a newer version
  // of this file. Either one alone leaves the user guessing the other.
  if (f.updateAvailable) {
    const up = document.createElement("span");
    up.className = "gv-upd";
    up.textContent = t("gv.state.update");
    meta.append(up);
  }
  const ev = document.createElement("span");
  ev.className = "gv-evidence";
  ev.textContent = gvEvidence(f);
  meta.append(ev);
}

// The two facts that let a user tell damage from a mod without opening anything: how the size
// compares to what was expected, and how long ago it changed. A file at a fraction of its stated
// length is a truncated download; a file changed last week is somebody's doing.
function gvEvidence(f) {
  if (f.state === "extraDir") return t("gv.nFiles", { n: f.files }) + " · " + fmtBytes(f.localSize);
  if (f.state === "extra") return fmtBytes(f.localSize) + " · " + fmtAge(f.mtime);
  // for a missing file the useful number is what restoring it will FETCH — there is nothing on
  // disk to describe, and a dash says less than nothing
  if (f.state === "missing") return fmtBytes(f.size);
  // `size` 0 means the payload carries no expectation to compare against (the update menu) —
  // showing "1.2 KB of 0 B" would be a comparison against nothing
  const size = f.size && f.localSize != null && f.localSize !== f.size
    ? t("gv.sizeOf", { have: fmtBytes(f.localSize), want: fmtBytes(f.size) })
    : fmtBytes(f.localSize);
  const age = fmtAge(f.mtime);
  return age ? size + " · " + age : size;
}

// ---- selection ----
function gvEachLeaf(n, fn) {
  for (const k of n.kids.values()) gvEachLeaf(k, fn);
  for (const f of n.visFiles) fn(f);
}

function gvToggleNode(n) {
  // A folder's box acts on what it is CURRENTLY showing — the same set its own count describes.
  // Acting on hidden rows would mean a click doing more than the row it sits on claims, and it is
  // also the safety line for extras: they are not shown until their chip is on, so no bulk
  // gesture can select one the user has not asked to see.
  const on = n.agg.sel < n.agg.vis;
  gvEachLeaf(n, (f) => {
    if (on) gv.sel.add(f.path);
    else gv.sel.delete(f.path);
  });
  gvRebuild();
}

// ---- what the selection costs, and what it decides ----
// Bundles make this non-additive: needing one member of a packed asset costs the whole asset, and
// needing a second member of the same one costs nothing more. The backend ships each row's
// `wireKey` precisely so this is a sum over DISTINCT keys rather than a round trip per click.
function gvTally() {
  const keys = new Map();
  let restore = 0;
  const del = [];
  const keep = [];
  for (const f of gv.data.files) {
    const picked = gv.sel.has(f.path);
    if (!gvRestorable(f)) {
      if (picked) del.push(f.path); // checking a file nobody owns means delete it
      continue;
    }
    if (picked) {
      restore++;
      if (f.wireKey) keys.set(f.wireKey, f.wire);
    } else if (f.state === "modified" || f.state === "kept") {
      // unchecked and different = "this one is mine". Independent of the filter: deselecting is a
      // deliberate act that stays made when the view changes around it.
      keep.push(f);
    }
  }
  let bytes = 0;
  for (const v of keys.values()) bytes += v;
  return { restore, keep, del, bytes };
}

// The message strip carries the view's standing warnings plus whatever just happened, so a
// transient result can never silently erase a condition that still holds.
// The screen's "working" state. Every act here is asynchronous, and pinning in particular fetches
// the shim manifest — so "Done" could sit for seconds on a live-looking screen and then navigate
// away by itself. This puts a spinner where the tally was, disables the two buttons, and makes the
// rows inert, so the view stops accepting decisions it has already sent.
function gvWorking(text) {
  gv.working = !!text;
  $("gv-work-text").textContent = text || "";
  $("gv-work").classList.toggle("hidden", !gv.working);
  $("gv-total").classList.toggle("hidden", gv.working);
  renderGvFooter();
}

function gvMsg(extra) {
  const text = [gv.warn, extra].filter(Boolean).join(" · ");
  $("gv-msg").textContent = text;
  $("gv-msg").classList.toggle("hidden", !text);
}

function renderGvFooter() {
  const tally = gvTally();
  const restoreBtn = $("btn-gv-restore"), delBtn = $("btn-gv-delete");
  // A button that cannot act is HIDDEN, not greyed. A disabled primary reading "Keep 0 as they
  // are" is the worst of both: it implies the screen has a main action and that the user has
  // failed to satisfy it, when in truth nothing here is theirs to restore.
  // In update mode the primary always acts: the release's other changes are installed either
  // way, and the ticks only decide which of the user's own files go with them. So it never hides
  // and never needs a "you must pick something" state.
  const update = gv.mode === "update";
  const canRestore = update || tally.restore > 0 || tally.keep.length > 0;
  restoreBtn.classList.toggle("hidden", !canRestore);
  restoreBtn.textContent = update
    ? t("gv.done")
    : tally.restore
    ? t("gv.restoreN", { n: tally.restore })
    : t("gv.keepN", { n: tally.keep.length });
  restoreBtn.disabled = state.busy || gv.working;
  restoreBtn.classList.toggle("danger", !update && !!gv.data.foreignBuild && tally.restore > 0);
  // Deleting is offered only when we could establish what Phoenix owns. If that lookup failed we
  // cannot tell the shim's files from anybody's, and offering to delete things we could not
  // identify is the same overclaim the rest of this screen exists to avoid.
  const canDelete = tally.del.length > 0 && !gv.data.phoenixUnknown;
  delBtn.classList.toggle("hidden", !canDelete);
  delBtn.textContent = t("gv.deleteN", { n: tally.del.length });
  delBtn.disabled = state.busy || gv.working;

  const parts = [];
  if (update) {
    // named for what the tick MEANS here: ticked = let the release replace my version
    parts.push(t("gv.totalOverwrite", { n: tally.restore }));
    if (tally.keep.length) parts.push(t("gv.totalKeep", { n: tally.keep.length }));
  } else {
    if (tally.restore) parts.push(t("gv.totalRestore", { n: tally.restore, size: fmtBytes(tally.bytes) }));
    if (tally.keep.length) parts.push(t("gv.totalKeep", { n: tally.keep.length }));
    if (tally.del.length) parts.push(t("gv.totalDelete", { n: tally.del.length }));
  }
  // With nothing picked and nothing to keep, say what the screen is waiting for rather than
  // leaving a bare row of Back. The wording follows what is actually on screen: where nothing is
  // restorable, ticking a row means DELETING it, and telling that user to "tick what to restore"
  // describes an action their results do not contain.
  const anyRestorable = gv.data.files.some(gvRestorable);
  $("gv-total").textContent = parts.length
    ? parts.join(" · ")
    : anyRestorable ? t("gv.hintPick") : t("gv.hintPickExtras");
}

function renderGvFacets() {
  const box = $("gv-facets");
  box.textContent = "";
  const counts = {};
  for (const f of gv.data.files) counts[gvKind(f)] = (counts[gvKind(f)] || 0) + 1;
  for (const k of GV_KINDS) {
    if (!counts[k]) continue;
    const b = document.createElement("button");
    b.className = "facet " + (gv.facets.has(k) ? "on" : "off");
    b.dataset.kind = k;
    b.innerHTML = '<span class="facet-dot"></span>';
    b.append(t("gv.facet." + k), Object.assign(document.createElement("span"), {
      className: "facet-n", textContent: String(counts[k]),
    }));
    b.addEventListener("click", () => {
      if (gv.facets.has(k)) gv.facets.delete(k);
      else gv.facets.add(k);
      renderGvFacets();
      gvRebuild();
    });
    box.append(b);
  }
}

// ---- open ----
function openFilesView(v, origin, mode) {
  gv.data = v;
  gv.mode = mode || "verify";
  gv.origin = origin || "main";
  gv.root = gvBuild(v.files);
  gv.sel = new Set();
  gv.filter = "";
  gv.cursor = 0;
  gvWorking(null);
  $("gv-filter").value = "";
  // Default: everything UNRULED-ON is checked. This keeps the plain-corruption case a two-click
  // job (open, Restore) while a decision the user already made is the one thing that survives
  // into the default state — `kept` because the pin still holds, and `superseded` because it only
  // stopped holding when the OTHER side changed. Ticking a superseded row by default would
  // silently reverse their answer on a screen they might not read closely.
  //
  // NOT in `yours` mode. There, every row is the user's own file by construction, so the same
  // default would open with a proposal to revert all of it — the one thing this screen must never
  // suggest. It opens with nothing selected and the extras chip ON, because unclaimed files are
  // half of what it exists to show. (The safety line that keeps a bulk gesture off rows the user
  // has not seen still holds: `gvEachLeaf` walks VISIBLE rows, and here they are visible.)
  if (gv.mode === "yours") {
    gv.facets = new Set(GV_KINDS);
  } else {
    for (const f of v.files) {
      if (gvRestorable(f) && f.state !== "kept") gv.sel.add(f.path);
    }
    // Extras are shown but OFF: they are usually the game's own droppings plus the user's mods, and
    // opening on a wall of files nobody is proposing to touch buries the ones that need a decision.
    gv.facets = new Set(GV_KINDS.filter((k) => k !== "extra"));
  }

  renderGvChrome();

  // Things the view must confess rather than paper over.
  const warn = [];
  if (v.foreignBuild) warn.push(t("gv.warnForeign", { version: v.version }));
  if (v.phoenixUnknown) warn.push(t("gv.warnPhoenix"));
  if (v.extrasTruncated) warn.push(t("gv.warnTruncated"));
  // kept, so a later message (a delete result) can be shown WITHOUT dropping a standing warning:
  // "this folder is a different build" does not stop being true because something was deleted
  gv.warn = warn.join(" ");
  gvMsg("");

  renderGvFacets();
  gvCompute(gv.root); // gvAutoOpen reads the aggregates
  gvAutoOpen();
  // MEASURE AFTER SHOWING. A hidden view is `display:none`, so anything inside it has no layout
  // and offsetHeight reads 0 — the probe silently fell back to a magic number that matched no
  // actual row height, and every row was then positioned on a pitch shorter than the rows
  // themselves. Overlapping rows, on a list whose whole job is to be scannable.
  showView("gv");
  gvMeasureRow();
  $("gv-area").scrollTop = 0;
  gvRebuild();
}

// Title + summary: everything on this screen whose WORDING depends on which question it is
// asking. Separate from openFilesView so a language change can re-render it without resetting
// the user's selection.
function renderGvChrome() {
  const v = gv.data;
  if (!v) return;
  $("gv-head").textContent = t(
    gv.mode === "update" ? "head.update" : gv.mode === "yours" ? "head.yours" : "head.files"
  );
  if (gv.mode === "yours") {
    // No integrity pass ran, so "4635 checked · 4635 intact" would be answering a question this
    // screen never asked — and claiming work it never did. It says what it actually holds.
    if (!v.files.length) {
      $("gv-summary").textContent = t("gv.yoursNone");
      return;
    }
    const bits = [t("gv.summaryYours", { n: v.files.length })];
    if (v.kept) bits.push(t("gv.summaryKept", { n: v.kept }));
    const extras = v.files.filter((f) => !gvRestorable(f)).length;
    if (extras) bits.push(t("gv.summaryExtras", { n: extras }));
    $("gv-summary").textContent = bits.join(" · ");
    return;
  }
  if (gv.mode === "update") {
    // The update menu is not a scan result: "4635 checked" would be answering a question nobody
    // asked. It states the only thing that matters here — this release wants to replace files
    // that are no longer ours, and which of them it replaces is about to be the user's call.
    $("gv-summary").textContent = t("gv.summaryUpdate", {
      n: v.files.filter((f) => f.state === "modified" || f.updateAvailable).length,
      kept: v.files.filter((f) => f.state === "kept").length,
    });
    return;
  }
  const bits = [t("gv.summary", { total: v.total, ok: v.ok })];
  if (v.kept) bits.push(t("gv.summaryKept", { n: v.kept }));
  if (v.skipped) bits.push(t("gv.summarySkipped", { n: v.skipped }));
  $("gv-summary").textContent = bits.join(" · ");
}

// What the tree looks like the moment it appears. A collapsed root is technically correct and
// practically useless — the screen opens on one row saying "game", and the user's first act is
// always the same click.
//
// Two regimes, because the two cases want opposite things. A handful of differences (the common
// verify) IS the answer, so it is shown whole — making somebody expand four folders to find three
// files is the tree working against them. A large one has to stay navigable, so only the spine
// opens: everything down to the first point where there is actually a choice to make.
const GV_OPEN_ALL_UNDER = 40;
function gvAutoOpen() {
  if (gv.root.agg.vis <= GV_OPEN_ALL_UNDER) {
    const all = (n) => {
      for (const k of n.kids.values()) { k.open = true; all(k); }
    };
    all(gv.root);
    return;
  }
  let n = gv.root;
  while (n.kids.size === 1 && n.visFiles.length === 0) {
    const [only] = n.kids.values();
    only.open = true;
    n = only;
  }
}

// The root font-size is fluid (it clamps on vmin), so a row's pixel height changes with the
// window. Virtual scrolling positions by index × height, so it has to be measured, never assumed.
function gvMeasureRow() {
  const probe = document.createElement("div");
  probe.className = "gv-row";
  probe.style.visibility = "hidden";
  $("gv-list").append(probe);
  const h = probe.offsetHeight;
  probe.remove();
  // A zero reading means the view was not laid out (it is hidden, or detached). Keep the last
  // known good height rather than inventing one: a wrong pitch is worse than a stale one, because
  // it is applied to every row at once.
  if (h > 0) gv.rowH = h;
}

// ---- committing the decision ----
async function gvApply() {
  const tally = gvTally();
  // Indexed, not searched. A foreign build selects ~4,600 rows, and a linear `find` per selected
  // path is 21 million string comparisons on the click that starts the repair.
  const byPath = new Map(gv.data.files.map((f) => [f.path, f]));
  const game = [], phx = [];
  for (const p of gv.sel) {
    const f = byPath.get(p);
    if (!f) continue;
    (f.owner === "phoenix" ? phx : game).push(p);
  }
  const keepGame = tally.keep.filter((f) => f.owner === "game").map((f) => f.path);
  const keepPhx = tally.keep.filter((f) => f.owner === "phoenix").map((f) => f.path);

  // A foreign build is not a repair — it is an overwrite of a working installation, so it keeps
  // its own wording and its own colour rather than hiding inside a routine confirm.
  const foreign = !!gv.data.foreignBuild && tally.restore > 0;
  // One statement per line, and only the ones that apply. Either half can be the whole answer:
  // unticking everything and pressing the primary ("Keep N as they are") is a legitimate way to
  // reach this dialog, and it used to open on "0 file(s) will be downloaded again (0 B)" — a
  // sentence about nothing, in front of the sentence that mattered. With nothing to restore the
  // title follows: this is the keep question, not a restore.
  const lines = [];
  if (foreign) lines.push(t("cf.foreignText", { n: tally.restore, version: gv.data.version }));
  else if (tally.restore) lines.push(t("cf.restoreText", { n: tally.restore, size: fmtBytes(tally.bytes) }));
  // stated in EVERY branch, including the foreign one: unticked rows are pinned by this button
  // whichever wording it wears, and the foreign copy used to leave that out entirely
  if (tally.keep.length) lines.push(t("cf.restoreKeep", { keep: tally.keep.length }));
  const ok = await confirmDialog({
    title: foreign ? t("cf.foreignTitle") : tally.restore ? t("cf.restoreTitle") : t("cf.keepTitle"),
    text: lines.join("\n"),
    confirm: foreign ? t("cf.foreignConfirm") : t("gv.confirmGo"),
    danger: foreign,
    // With nothing to restore, this dialog's entire output is durable pins — and "Select none"
    // then the primary reaches it with every row in it, so the Enter-able answer must be the one
    // that changes nothing. Same rule gvBack's keep dialog follows, for the same gesture.
    defaultCancel: !foreign && !tally.restore,
  });
  if (!ok) return;

  // Pins first, and under the busy token — they are writes. `game_repair` records the game half's
  // own pins when it actually repairs something, so that case is left to it below.
  if (keepPhx.length || keepGame.length) {
    const busy = acquireBusy();
    if (busy == null) return; // an op grabbed the UI in between — stay rather than lose the answer
    gvWorking(t("gv.keeping"));
    try {
      if (keepPhx.length) await invoke("phoenix_keep", { keep: keepPhx });
      // A game-side decision with nothing to fetch is not a repair: the command pins and returns.
      // It used to be routed through the download modal anyway, so the commonest gesture in the
      // Your-files view ("Keep N as they are") flashed a "Game repair" dialog over a run that
      // transferred nothing and then reported "Game files intact · 0 game file(s) restored".
      if (keepGame.length && !game.length) {
        await invoke("game_repair", { restore: [], keep: keepGame });
      }
    } catch (e) {
      // gvMsg, NOT onError: onError writes main's status line, which is behind this view. The
      // dialog would close over an unchanged screen and the decision would be silently lost.
      gvMsg(errText(e));
      gvWorking(null);
      releaseBusy(busy);
      return;
    }
    gvWorking(null);
    releaseBusy(busy);
  }
  if (game.length) {
    // the Phoenix half rides along and runs after — one selection, one confirm, both pipelines
    startGameRepair(gv.data, game, keepGame, phx);
    return; // the download modal owns the screen from here; it lands back on main itself
  }
  if (phx.length) {
    showView("main");
    doApply(phx);
    return;
  }
  // keeps only — nothing downloads, so say so where every other verdict is said, in the words of
  // what actually happened rather than a repair's
  showView("main");
  setStatus(t("status.yourFiles"), "ok", t("gv.keptOnly", { n: tally.keep.length }));
  doCheck();
}

async function gvDelete() {
  const paths = gvTally().del;
  if (!paths.length) return;
  const ok = await confirmDialog({
    title: t("cf.deleteTitle"),
    text: t("cf.deleteText", { n: paths.length }),
    confirm: t("cf.deleteConfirm"),
    danger: true, // nothing here can be undone, and nothing else in this app can restore it
  });
  if (!ok) return;
  const busy = acquireBusy();
  if (busy == null) return;
  gvWorking(t("gv.deleting", { n: paths.length }));
  try {
    const n = await invoke("game_delete_extras", { paths });
    // the rows are gone for real — drop them from the model rather than re-verifying 15 GB
    const gone = new Set(paths);
    gv.data.files = gv.data.files.filter((f) => !gone.has(f.path));
    for (const p of gone) gv.sel.delete(p);
    gv.root = gvBuild(gv.data.files);
    renderGvFacets();
    // the summary describes the same list, so it has to move with it — deleting every extra
    // otherwise left "12 files of your own · 7 nothing claims" printed above rows that no longer
    // contained a single one
    renderGvChrome();
    // rebuilding the tree makes fresh nodes, so the open state has to be re-established — without
    // this the list collapsed to its roots the moment anything was deleted
    gvCompute(gv.root);
    gvAutoOpen();
    gvRebuild();
    gvMsg(t("gv.deleted", { n }));
  } catch (e) {
    gvMsg(errText(e));
  } finally {
    gvWorking(null);
    releaseBusy(busy);
  }
}

// ---- wiring ----
$("gv-area").addEventListener("scroll", () => renderGv(), { passive: true });
window.addEventListener("resize", () => {
  if (currentView() !== "gv") return;
  gvMeasureRow();
  renderGv();
});
$("gv-list").addEventListener("click", (e) => {
  if (gv.working) return; // the answer is already on its way to the backend
  const row = e.target.closest(".gv-row");
  if (!row) return;
  const item = gv.flat[Number(row.dataset.i)];
  if (!item) return;
  if (e.target.closest(".gv-box")) {
    if (item.node) gvToggleNode(item.node);
    else {
      if (gv.sel.has(item.file.path)) gv.sel.delete(item.file.path);
      else gv.sel.add(item.file.path);
      gvRebuild();
    }
    return;
  }
  // anywhere else on a folder row toggles it open: the chevron is a 14px target and the row is
  // the thing the user is actually pointing at
  if (item.node) {
    item.node.open = !item.node.open;
    gvRebuild();
  }
});

// ---- keyboard ----
// One roving tab stop over a virtualised list. `gv.cursor` is an index into `gv.flat`, not an
// element: the row elements are a recycled pool, so the focused DOM node is whatever currently
// renders that index, and it has to be re-focused after every rebuild.
function gvFocus(i) {
  if (!gv.flat.length) return;
  gv.cursor = Math.max(0, Math.min(gv.flat.length - 1, i));
  // scroll it into the window BEFORE focusing: a row outside the virtual window has no element
  const top = gv.cursor * gv.rowH;
  const area = $("gv-area");
  if (top < area.scrollTop) area.scrollTop = top;
  else if (top + gv.rowH > area.scrollTop + area.clientHeight) {
    area.scrollTop = top + gv.rowH - area.clientHeight;
  }
  renderGv();
  $("gv-list").querySelector(`.gv-row[data-i="${gv.cursor}"]`)?.focus();
}
$("gv-list").addEventListener("keydown", (e) => {
  if (gv.working || !gv.flat.length) return;
  const item = gv.flat[gv.cursor];
  const step = { ArrowDown: 1, ArrowUp: -1 }[e.key];
  if (step != null) { e.preventDefault(); gvFocus(gv.cursor + step); return; }
  if (e.key === "Home" || e.key === "End") {
    e.preventDefault();
    gvFocus(e.key === "Home" ? 0 : gv.flat.length - 1);
    return;
  }
  if (!item) return;
  // Space ticks (the checkbox act), Enter/Right/Left work the folder — the same split a tree
  // widget uses, so the two acts a row carries never collide on one key
  if (e.key === " ") {
    e.preventDefault();
    if (item.node) gvToggleNode(item.node);
    else {
      if (gv.sel.has(item.file.path)) gv.sel.delete(item.file.path);
      else gv.sel.add(item.file.path);
      gvRebuild();
    }
    gvFocus(gv.cursor);
    return;
  }
  if (item.node && (e.key === "Enter" || e.key === "ArrowRight" || e.key === "ArrowLeft")) {
    e.preventDefault();
    item.node.open = e.key === "ArrowLeft" ? false : e.key === "ArrowRight" ? true : !item.node.open;
    gvRebuild();
    gvFocus(gv.cursor);
  }
});
// clicking a row moves the cursor there, so the keyboard picks up where the mouse left off
$("gv-list").addEventListener("mousedown", (e) => {
  const row = e.target.closest(".gv-row");
  if (row) gv.cursor = Number(row.dataset.i);
});
$("gv-filter").addEventListener("input", (e) => {
  gv.filter = e.target.value.trim().toLowerCase();
  $("gv-area").scrollTop = 0;
  gvRebuild();
});
// Bulk controls act on what is VISIBLE — which is also the safety line for extras, since those
// are not shown until their chip is turned on.
$("gv-all").addEventListener("click", () => {
  gvEachLeaf(gv.root, (f) => gv.sel.add(f.path));
  gvRebuild();
});
$("gv-none").addEventListener("click", () => {
  gvEachLeaf(gv.root, (f) => gv.sel.delete(f.path));
  gvRebuild();
});
$("btn-gv-restore").addEventListener("click", () => (gv.mode === "update" ? gvUpdateApply() : gvApply()));
$("btn-gv-delete").addEventListener("click", gvDelete);
// Back means leave. The pins are a SEPARATE question, and it is only worth asking when the user
// actually made a decision that leaving would drop: Restore is what commits them, and someone who
// unticked their mods and then reached for Back would otherwise be told nothing and see the same
// files reported again next time. Escape/"Don't remember" leaves without pinning — the status quo
// — because pressing Back already said "leave"; this dialog is only about what to carry out.
async function gvBack() {
  // A `kept` row unticked is already pinned; nothing new was decided. A `superseded` one IS a
  // fresh decision even though it was pinned before — the stored pin names a release version that
  // no longer exists, so leaving it unwritten means being asked again next time.
  // A `kept` row whose pin still holds is already recorded; one the release has OUTRUN is not —
  // its pin names a version that no longer exists, so leaving it unwritten means being asked
  // again next time.
  const pending = gvTally().keep.filter((f) => f.state === "modified" || f.updateAvailable);
  if (!pending.length) { showView(gv.origin); return; }
  const ok = await confirmDialog({
    title: t("cf.keepTitle"),
    text: t("cf.keepText", { n: pending.length }),
    confirm: t("cf.keepConfirm"),
    cancel: t("cf.keepDiscard"),
    // Remembering writes durable state, and "Select none" then Back can reach this with every
    // row in it — so the focused, Enter-able answer is the one that changes nothing.
    defaultCancel: true,
  });
  if (ok) {
    const busy = acquireBusy();
    if (busy == null) return; // an op grabbed the UI in between — stay rather than lose the answer
    gvWorking(t("gv.keeping"));
    const game = pending.filter((f) => f.owner === "game").map((f) => f.path);
    const phx = pending.filter((f) => f.owner === "phoenix").map((f) => f.path);
    try {
      // an empty `restore` with a non-empty `keep` is a first-class call: pin, download nothing
      if (game.length) await invoke("game_repair", { restore: [], keep: game });
      if (phx.length) await invoke("phoenix_keep", { keep: phx });
    } catch (e) {
      // Stay. Leaving on a failed write would discard the decision silently, which is the exact
      // thing this dialog exists to prevent.
      gvMsg(errText(e));
      gvWorking(null);
      releaseBusy(busy);
      return;
    }
    gvWorking(null);
    releaseBusy(busy);
    // pinning a Phoenix file moves it out of `Modified`, which is what the main view counts
    if (phx.length) doCheck();
  }
  showView(gv.origin);
}
$("btn-gv-back").addEventListener("click", gvBack);

// ---- update menu: which of MY files may this release replace? ----
// Same tree, same ticks, same pinning as the verify view — because it is the same question asked
// from the other side. A row here is a Phoenix file whose bytes are no longer the ones we wrote:
// tick it to take the release's version, leave it to keep yours (which pins it).
function openUpdateMenu(c) {
  const files = c.files
    // `yours` rows are pins on dests the SHIM does not manage — a vanilla file somebody modded.
    // They ride in the same array so the managed-files list can show them, and they have no
    // business here: this menu is one release's offer, every row in it is hard-coded
    // `owner: "phoenix"`, and both answers were wrong for them. Ticking one promised the release's
    // version and did nothing at all (`install(only)` plans over the shim manifest, which has no
    // such dest); leaving one unticked sent it to `phoenix_keep`, which knows no `theirs` for it
    // and so REPLACED a two-sided pin with a one-sided one — the pin then holds forever and the
    // file silently stops receiving updates, which is the exact failure `theirs` exists to prevent.
    .filter((f) => (f.status === "modified" || f.status === "kept") && !f.yours)
    .map((f) => ({
      path: f.dest,
      owner: "phoenix",
      state: f.status,
      // no manifest size on this payload; gvEvidence falls back to the local size alone
      size: 0,
      localSize: f.localSize ?? null,
      mtime: f.mtime ?? null,
      updateAvailable: !!f.updateAvailable,
      wireKey: null,
      wire: 0,
      files: 0,
    }));
  openFilesView(
    {
      version: c.version, total: c.files.length, ok: 0, skipped: 0,
      kept: files.filter((f) => f.state === "kept").length,
      files, damagedBytes: 0, extrasTruncated: false,
      foreignBuild: false, phoenixUnknown: false,
    },
    currentView(),
    "update"
  );
}

// Run the release, carrying the user's answer about the contested files.
async function gvUpdateApply() {
  const tally = gvTally();
  const keep = tally.keep.map((f) => f.path);
  // Everything the release does regardless — install, update, remove. `install(only)` acts on
  // exactly the dests it is given, so the unattended work has to be named alongside the ticked
  // files; omitting it would turn "Update" into "only overwrite my edited files".
  const auto = (state.lastCheck ? state.lastCheck.files : [])
    .filter((f) => f.status === "update" || f.status === "install" || f.status === "remove")
    .map((f) => f.dest);
  if (keep.length) {
    const busy = acquireBusy();
    if (busy == null) return;
    gvWorking(t("gv.keeping"));
    try {
      // pinned BEFORE the install, so a failed download cannot lose the "keep mine" half
      await invoke("phoenix_keep", { keep });
    } catch (e) {
      gvMsg(errText(e));
      gvWorking(null);
      releaseBusy(busy);
      return;
    }
    gvWorking(null);
    releaseBusy(busy);
  }
  showView("main");
  const work = [...auto, ...gv.sel];
  if (!work.length) {
    // Nothing to install: no release changes pending, and every contested file was left as the
    // user's. The pins above ARE the outcome, so skip the apply rather than round-tripping to
    // GitHub to write an unchanged state file — and re-check so the main view reflects the new
    // decisions (those files are `kept` now, not `modified`).
    doCheck();
    return;
  }
  doApply(work);
}

// ---- confirm modal (destructive actions: uninstall, discard autoexec changes) ----
let cfResolve = null;
// `danger` paints the confirm terracotta instead of gold — reserved for the two irreversible
// acts (uninstall, overwriting a foreign build). Kept rare on purpose: a red button that shows up
// for "discard my edits" stops meaning anything.
function confirmDialog({ title, text, confirm, cancel, danger, defaultCancel }) {
  $("cf-title").textContent = title;
  $("cf-text").textContent = text;
  $("btn-cf-ok").textContent = confirm;
  // `cancel` names the OTHER outcome when it is a real choice rather than a way out — "Don't
  // remember" is an answer to the question; "Cancel" would be an answer to a different one. The
  // label is transient (applyStatic restores it on a language switch, which cannot happen while
  // a modal holds the stage inert).
  $("btn-cf-cancel").textContent = cancel || t("btn.cancel");
  $("btn-cf-ok").classList.toggle("danger", !!danger);
  $("cf-modal").classList.remove("hidden");
  // keyboard path: Enter takes the focused answer, Escape cancels, Tab reaches the other one.
  // `defaultCancel` moves that focus for dialogs whose confirm creates state rather than
  // completing an action the user already started.
  (defaultCancel ? $("btn-cf-cancel") : $("btn-cf-ok")).focus();
  return new Promise((resolve) => { cfResolve = resolve; });
}
function settleConfirm(v) {
  if ($("cf-modal").classList.contains("hidden")) return;
  $("cf-modal").classList.add("hidden");
  const r = cfResolve;
  cfResolve = null;
  if (r) r(v);
}

// ---- customization ----
// Controls react optimistically: the class flips in place (so CSS transitions play), then the
// selection persists + the diff refreshes in the background, serialized on one promise chain.
let selChain = Promise.resolve();

// `revert` puts the control back the way it was: the flip is optimistic, so a failure that only
// logged left the switch reading "on" for a selection that was never saved — and the next install
// would quietly ship the old variant. The message goes to the OPTIONS view's own plate; onError
// paints the main view, which is hidden while this one is up.
function queueSelection(id, value, revert) {
  selChain = selChain.then(async () => {
    try {
      await invoke("set_selection", { id, value });
      setOptsMsg(null);
      await doReplan(); // cached manifest, no network
    } catch (e) {
      revert?.();
      setOptsMsg(errText(e));
    }
  });
}

function setOptsMsg(text) {
  const m = $("opts-msg");
  if (text) { m.textContent = text; m.hidden = false; } else { m.hidden = true; }
}

function renderOptions() {
  const wrap = $("opts");
  wrap.innerHTML = "";
  for (const o of state.lastCheck?.options ?? []) {
    const g = document.createElement("div");
    g.className = "opt-group";

    const head = document.createElement("div");
    head.className = "opt-head";
    const title = document.createElement("span");
    title.className = "opt-title";
    title.textContent = mlabel(o.label);
    head.append(title);

    if (o.kind === "toggle") {
      const sw = document.createElement("button");
      sw.className = "switch" + (o.value === true ? " on" : "");
      sw.setAttribute("role", "switch");
      sw.setAttribute("aria-checked", String(o.value === true));
      sw.addEventListener("click", () => {
        if (state.busy) return;
        const was = o.value === true;
        o.value = !was;
        sw.classList.toggle("on", o.value);
        sw.setAttribute("aria-checked", String(o.value));
        queueSelection(o.id, o.value, () => {
          o.value = was;
          sw.classList.toggle("on", was);
          sw.setAttribute("aria-checked", String(was));
        });
      });
      head.append(sw);
    }
    g.append(head);

    if (o.description) {
      const d = document.createElement("div");
      d.className = "hint";
      d.textContent = mlabel(o.description);
      g.append(d);
    }

    if (o.kind === "choice") {
      const list = document.createElement("div");
      list.className = "opt-variants";
      for (const v of o.variants) {
        const row = document.createElement("button");
        row.className = "variant" + (o.value === v.id ? " active" : "");
        const dot = document.createElement("span");
        dot.className = "radio";
        const lb = document.createElement("span");
        lb.textContent = mlabel(v.label);
        row.append(dot, lb);
        row.addEventListener("click", () => {
          if (state.busy || o.value === v.id) return;
          const was = o.value;
          o.value = v.id;
          for (const r of list.children) r.classList.toggle("active", r === row);
          queueSelection(o.id, v.id, () => {
            o.value = was;
            for (const [i, r] of [...list.children].entries()) {
              r.classList.toggle("active", o.variants[i]?.id === was);
            }
          });
        });
        list.append(row);
      }
      g.append(list);
    }

    wrap.append(g);
  }
}

// ---- autoexec editor ----
function setAeMsg(text) {
  const m = $("ae-msg");
  if (text) { m.textContent = text; m.hidden = false; } else { m.hidden = true; }
}

function setAeDirty(d) {
  state.aeDirty = d;
  $("ae-dirty").classList.toggle("hidden", !d);
}

// Source cfg highlight: `<command> <valueвЂ¦>` per line, `//` comments, "quoted" values.
function hlAutoexec(text) {
  const lines = text.split(/\r?\n/).map((line) => {
    // a comment starts at the first // OUTSIDE quotes (a // inside a quoted value, e.g. a
    // URL, stays value)
    let ci = -1;
    let quoted = false;
    for (let i = 0; i < line.length - 1; i++) {
      if (line[i] === '"') quoted = !quoted;
      else if (!quoted && line[i] === "/" && line[i + 1] === "/") { ci = i; break; }
    }
    const code = ci >= 0 ? line.slice(0, ci) : line;
    const com = ci >= 0 ? `<span class="hl-com">${escHtml(line.slice(ci))}</span>` : "";
    const m = code.match(/^(\s*)(\S*)([\s\S]*)$/);
    let html = m[1];
    if (m[2]) html += `<span class="hl-cmd">${escHtml(m[2])}</span>`;
    // the value part: quoted runs colored, the rest plain
    m[3].replace(/("[^"]*"?)|([^"]+)/g, (_, str, plain) => {
      html += str ? `<span class="hl-str">${escHtml(str)}</span>` : escHtml(plain);
    });
    return html + com;
  });
  return lines.join("\n") + "\n"; // trailing newline keeps pre/textarea heights in step
}

function refreshAeHl() {
  const ta = $("ae-text");
  const hl = $("ae-hl");
  hl.innerHTML = hlAutoexec(ta.value);
  hl.scrollTop = ta.scrollTop;
  hl.scrollLeft = ta.scrollLeft;
}

async function openAutoexec() {
  // read-only mode protects the file: a lossy (non-UTF-8) decode or a failed read must never
  // be saved back — that would corrupt or blank the user's real cfg
  let msg = null;
  let readOnly = false;
  try {
    const r = await invoke("read_autoexec");
    $("ae-text").value = r.content;
    if (r.lossy) { readOnly = true; msg = t("ae.lossy"); }
  } catch (e) {
    $("ae-text").value = "";
    readOnly = true;
    msg = errText(e);
  }
  state.aeReadOnly = readOnly;
  $("ae-text").readOnly = readOnly;
  $("btn-ae-save").disabled = readOnly;
  refreshAeHl();
  setAeDirty(false);
  setAeMsg(msg);
  showView("autoexec");
}

async function saveAutoexec() {
  if (state.aeReadOnly) return;
  try {
    await invoke("save_autoexec", { content: $("ae-text").value });
    setAeDirty(false);
    setAeMsg(null);
    showView("settings"); // saved = done editing; a failure stays open with the message
  } catch (e) {
    setAeMsg(errText(e));
  }
}

async function maybeCloseAutoexec() {
  if (state.aeDirty) {
    const ok = await confirmDialog({
      title: t("cf.discardTitle"),
      text: t("cf.discardText"),
      confirm: t("cf.discardConfirm"),
    });
    if (!ok) return;
    setAeDirty(false);
  }
  showView("settings");
}

// ---- what's new ----
// TWO histories on two pages, because they are two products on two version lines: the Phoenix
// client (notes carried in each dist release's manifest) and the launcher itself (notes carried as
// each GitHub release's description). Merged into one list they were unreadable — two different
// "1.3.5" entries in one column, and no way to tell which thing a change was about.
//
// Per pane: which element holds it, which command fills it, a `seed` (a release we already know
// about, painted instantly so the page is never blank while the history loads), and `here` — the
// version this machine is actually on, which gets the "current" pill. `loaded` = real history is
// painted; `atTop` = its scroll reset has been carried out (it can only happen while visible).
const WN = {
  phoenix: {
    body: "notes-body", msg: "wn-msg-phoenix", cmd: "release_notes",
    seq: 0, loaded: false, atTop: false,
    seed: () => (state.lastCheck?.notes
      ? { version: state.lastCheck.version, notes: state.lastCheck.notes } : null),
    // the INSTALLED version, and only when it is what is actually on disk: with changes pending, a
    // check's `version` names the latest release instead — the same rule the foot line follows
    here: () => {
      const c = state.lastCheck;
      return c?.installed && c.version && (c.local || c.changes === 0) ? c.version : null;
    },
  },
  launcher: {
    body: "wn-launcher-body", msg: "wn-msg-launcher", cmd: "launcher_notes",
    seq: 0, loaded: false, atTop: false,
    seed: () => (state.launcherUpdate?.notes
      ? { version: state.launcherUpdate.version, notes: state.launcherUpdate.notes } : null),
    here: () => state.launcherVersion,
  },
};

// The pane's trailing line: loading, offline, or "nothing yet". One element per pane rather than
// an appended node, so repeated failures can't stack up copies of themselves.
function wnMsg(name, text) {
  const m = $(WN[name].msg);
  m.textContent = text || "";
  m.classList.toggle("hidden", !text);
}

// One version section: mono version line (+ a "current" pill when this is the build in use) and
// the rendered notes.
function notesSection(version, notes, current) {
  const frag = document.createDocumentFragment();
  const h = document.createElement("div");
  h.className = "whatsnew-version";
  h.append("v" + bareVer(version));
  if (current) {
    const tag = document.createElement("span");
    tag.className = "tag current";
    tag.textContent = t("wn.current");
    h.append(tag);
  }
  const n = document.createElement("div");
  n.className = "notes";
  n.innerHTML = renderNotes(notes);
  frag.append(h, n);
  return frag;
}

// Fill one pane. Both commands are cached backend-side (memory + disk), so a revisit is a local
// round trip — worth paying every time, because a check that found a new release must show up
// here without restarting the app. `loaded` means real history is already painted: it keeps a
// revisit from flashing back to the seed, and a failed refetch from wiping what is on screen.
async function loadWnPane(name) {
  const p = WN[name];
  const seq = ++p.seq; // drops a stale fetch when the pane was re-entered meanwhile
  const body = $(p.body);
  if (!p.loaded) {
    body.innerHTML = "";
    const s = p.seed();
    if (s) body.append(notesSection(s.version, s.notes, sameVer(s.version, p.here())));
    wnMsg(name, t("wn.loading"));
  }
  try {
    const all = await invoke(p.cmd);
    if (seq !== p.seq) return;
    if (all.length) {
      // read AFTER the await: a check landing while this was in flight changes which version the
      // folder is on, and the pill would otherwise be one visit behind
      const here = p.here();
      body.innerHTML = "";
      for (const e of all) body.append(notesSection(e.version, e.notes, sameVer(e.version, here)));
      p.loaded = true;
      wnMsg(name, "");
    } else {
      // No history. A SEED stays — it is the release the check just told us about, so it is real
      // notes about this repo. A previously loaded history does not: an empty answer now means
      // the repo it came from is not the one being asked about any more (the source repo can be
      // changed in settings), and showing another repo's changelog is worse than showing none.
      if (p.loaded) { body.innerHTML = ""; p.loaded = false; }
      wnMsg(name, body.firstChild ? "" : t("wn.none"));
    }
  } catch (e) {
    // offline etc. — whatever is painted stays up; say why the rest is missing
    if (seq === p.seq) wnMsg(name, errText(e));
  }
}

// Which history is showing. Remembered for the session, like settings' tab: a repeat visit lands
// where the user was.
function setWnTab(name) {
  state.wnTab = name;
  for (const b of $("wn-tabs").querySelectorAll(".tab")) {
    const on = b.dataset.wn === name;
    b.classList.toggle("active", on);
    b.setAttribute("aria-selected", String(on));
  }
  for (const p of $("view-whatsnew").querySelectorAll("[data-wn-pane]")) {
    const on = p.dataset.wnPane === name;
    p.classList.toggle("hidden", !on);
    // It has a layout box again, so a pending "start at the top" can finally be honoured — see
    // openWhatsNew for why it could not be done there.
    if (on && !WN[name].atTop) {
      p.scrollTop = 0;
      WN[name].atTop = true;
    }
  }
  loadWnPane(name);
}

function openWhatsNew() {
  // Entering the view starts both pages at the top — nobody means to land half-way down their own
  // last visit. The reset is DEFERRED to setWnTab, per pane, because `scrollTop` only takes while
  // the element has a layout box: assigning it through a `display:none` ancestor is silently
  // ignored, and Chromium hands the old offset straight back when the pane returns. Done here,
  // with the view itself still hidden, it reset nothing at all.
  for (const k in WN) WN[k].atTop = false;
  showView("whatsnew");
  setWnTab(state.wnTab);
}

// ---- wire ----
// Expanding a category is a pure re-render of the payload already on screen — no check, no
// network. Refused while an op runs: the rows carry live download state (bar widths, `_pending`,
// the settled `done` class) that exists only in the DOM, and rebuilding them mid-apply would
// restart every bar from whatever the ORIGINAL plan said.
function toggleFileCat(id) {
  if (state.busy || !state.filesShown) return;
  if (state.filesOpen.has(id)) state.filesOpen.delete(id);
  else state.filesOpen.add(id);
  renderFiles(state.filesShown);
  // keep the keyboard on the row that was just toggled, not back at the top of the document
  $("files").querySelector(`li.fcat[data-cat="${CSS.escape(id)}"]`)?.focus();
}
$("files").addEventListener("click", (e) => {
  const li = e.target.closest("li.fcat");
  if (li) toggleFileCat(li.dataset.cat);
});
$("files").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  const li = e.target.closest("li.fcat");
  if (!li) return;
  e.preventDefault(); // Space would scroll the list out from under the row
  toggleFileCat(li.dataset.cat);
});

$("btn-primary").addEventListener("click", onPrimary);
$("btn-check").addEventListener("click", () => !state.busy && doCheck());
// Uninstall lives in settings' game-files tab but reports through the status line on MAIN, so it
// leaves settings first — same guard Verify uses. Order differs on purpose: the destructive
// confirm comes BEFORE the unsaved-changes guard, so backing out of the confirm leaves the user
// exactly where they were instead of dumping them on main.
$("btn-uninstall").addEventListener("click", async () => {
  if (state.busy) return;
  const ok = await confirmDialog({
    title: t("cf.uninstallTitle"),
    text: t("cf.uninstallText"),
    confirm: t("cf.uninstallConfirm"),
    danger: true,
  });
  if (!ok) return;
  if (currentView() === "settings" && !(await maybeCloseSettings())) return;
  doUninstall();
});
$("settings-tabs").addEventListener("click", (e) => {
  const b = e.target.closest(".tab");
  if (b) setSettingsTab(b.dataset.tab);
});
// The busy guard every other nav control carries — and re-picking the tab already showing must
// not spend a fetch re-answering what is on screen.
$("wn-tabs").addEventListener("click", (e) => {
  const b = e.target.closest(".tab");
  if (b && !state.busy && b.dataset.wn !== state.wnTab) setWnTab(b.dataset.wn);
});
$("btn-settings").addEventListener("click", () => !state.busy && openSettings());
$("btn-whatsnew").addEventListener("click", () => !state.busy && openWhatsNew());
$("btn-customize").addEventListener("click", () => { if (!state.busy) { renderOptions(); showView("options"); } });
$("btn-options-back").addEventListener("click", () => showView("main"));
$("btn-whatsnew-back").addEventListener("click", () => showView("main"));
$("btn-save").addEventListener("click", saveSettings);
$("btn-back").addEventListener("click", () => maybeCloseSettings());
$("btn-browse").addEventListener("click", () => browseInto($("in-game")));
$("btn-token-clear").addEventListener("click", () => {
  state.tokenClear = true;
  state.hasToken = false;
  $("in-token").value = "";
  updateTokenPlaceholder();
  $("btn-token-clear").classList.add("hidden");
});
// Same guard Verify carries: it reports through the files view, which Back must return to
// settings, and it must not start while another op owns the folder.
$("btn-yours").addEventListener("click", () => {
  if (!state.busy && !state.gameRunning) doYourFiles("settings");
});
$("btn-autofind").addEventListener("click", () => openAutofind("settings"));
$("btn-setup-browse").addEventListener("click", setupBrowse);
$("btn-setup-autofind").addEventListener("click", () => openAutofind("setup"));
$("btn-setup-download").addEventListener("click", () => startGameDownload("setup"));

// ---- game download / repair modal ----
// Like Verify and Uninstall, this ends on main (gdRun's success path calls showView) — so it goes
// through the same unsaved-changes guard first. Without it a successful download silently threw
// away edits made in another settings tab.
$("btn-fresh").addEventListener("click", async () => {
  if (currentView() === "settings" && !(await maybeCloseSettings())) return;
  startGameDownload("settings");
});
// Verify reports through the status line, which lives on MAIN — so leave settings first (through
// the same unsaved-changes guard as Back) rather than running a minute-long scan the user can't
// see. A cancelled guard means the user chose to keep editing: don't start the scan either.
// Stays put: the progress modal opens on top of Settings and the files view returns to it.
$("btn-verify").addEventListener("click", () => doGameVerify(currentView()));
// the destination stage: the switch, the one editable path segment, and the two ways out
$("gd-nest").addEventListener("click", () => {
  gdDest.nest = !gdDest.nest;
  gdDestRefresh();
});
$("gd-path-name").addEventListener("input", (e) => {
  gdDest.name = e.target.value;
  gdDestRefresh();
});
$("gd-path-name").addEventListener("keydown", (e) => {
  // Enter from the field means the primary. The global handler leaves keys other than Escape alone
  // while a modal is open, so nothing else would act on it.
  if (e.key !== "Enter" || $("btn-gd-dest-go").disabled) return;
  e.preventDefault();
  $("btn-gd-dest-go").click();
});
$("btn-gd-dest-go").addEventListener("click", () => {
  if (gdDest.path) gdOpen(gdDest.origin, gdDest.path);
});
$("btn-gd-dest-cancel").addEventListener("click", () => gdClose());
$("btn-gd-go").addEventListener("click", () => gdRun());
$("btn-gd-close").addEventListener("click", () => gdClose());
$("btn-gd-err-close").addEventListener("click", () => gdClose());
$("btn-gd-retry").addEventListener("click", () => gdRun()); // resumes: done files skip, .parts continue
$("btn-gd-cancel").addEventListener("click", () => invoke("game_cancel").catch(() => {}));
$("btn-af-go").addEventListener("click", runAutofind);
$("btn-af-close").addEventListener("click", closeAutofind);
$("btn-af-cancel").addEventListener("click", cancelAutofind);
$("btn-af-done").addEventListener("click", closeAutofind);
$("btn-autoexec").addEventListener("click", openAutoexec);
$("btn-ae-save").addEventListener("click", saveAutoexec);
$("btn-ae-close").addEventListener("click", maybeCloseAutoexec);
$("btn-vf-close").addEventListener("click", () => $("vf-modal").classList.add("hidden"));
$("btn-cf-ok").addEventListener("click", () => settleConfirm(true));
$("btn-cf-cancel").addEventListener("click", () => settleConfirm(false));
wireSeg($("seg-lang"), (l) => switchLang(l));
wireSeg($("seg-renderer"));
// instant-apply (persist is best-effort — the visual change already happened, and the next
// save/boot converges); excluded from the settings snapshot like language
wireSeg($("seg-anim"), (v) => {
  applyAnimations(v === "on");
  invoke("set_animations", { on: v === "on" }).catch(() => {});
});

// Watch the modals' own class rather than calling syncModalLayer from all seven open/close
// sites — this cannot be forgotten when a new dialog is added.
const modalObserver = new MutationObserver(syncModalLayer);
for (const m of document.querySelectorAll(".modal")) {
  modalObserver.observe(m, { attributes: true, attributeFilter: ["class"] });
}

// The WebView2 default context menu (Back / Reload / Save as / Print) is a browser artifact: none
// of it means anything here, and half of it would break the illusion that this is an app. Editable
// fields keep theirs — it is the only mouse route to Paste, and a GitHub token is exactly the kind
// of string nobody types by hand. Read-only ones (the autoexec editor after a lossy decode) keep it
// too, so Copy still works there.
document.addEventListener("contextmenu", (e) => {
  const tag = e.target && e.target.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || (e.target && e.target.isContentEditable)) return;
  e.preventDefault();
});

// Escape backs out (topmost layer first); Enter in settings commits the save. The confirm
// modal owns the keyboard while open: Enter = confirm (or cancel, if Cancel is focused),
// Escape = cancel, everything else stays inside it.
document.addEventListener("keydown", (e) => {
  // A modal owns the keyboard. Without this the Enter-commits-settings branch below fired while
  // the autofind or download dialog was up — settings saved and closed BEHIND the dialog, and an
  // autofind result the user had waited minutes for was then written into a form nobody was
  // looking at and discarded on the next open.
  const modal = openModal();
  if (modal && modal !== $("cf-modal")) {
    if (e.key !== "Escape") return;
  }
  if (!$("cf-modal").classList.contains("hidden")) {
    if (e.key === "Escape") settleConfirm(false);
    else if (e.key === "Enter") { e.preventDefault(); settleConfirm(e.target !== $("btn-cf-cancel")); }
    return;
  }
  if (e.key === "Escape") {
    if (!$("gd-modal").classList.contains("hidden")) {
      // mid-download: Escape asks the backend to stop (typed cancel closes the modal quietly);
      // any other stage just closes
      if (!$("gd-run").classList.contains("hidden")) invoke("game_cancel").catch(() => {});
      else gdClose();
      return;
    }
    if (!$("af-modal").classList.contains("hidden")) {
      if (!$("af-run").classList.contains("hidden")) cancelAutofind();
      closeAutofind();
      return;
    }
    if (!$("vf-modal").classList.contains("hidden")) {
      // mid-verify Escape asks for the stop (the Stop button is the visible route, Escape the one
      // a keyboard reaches for first); on the verdict stage it just closes
      if (!$("vf-run").classList.contains("hidden")) $("btn-vf-stop").click();
      else $("vf-modal").classList.add("hidden");
      return;
    }
    const v = currentView();
    if (v === "autoexec") { e.preventDefault(); maybeCloseAutoexec(); }
    else if (v === "settings") { e.preventDefault(); maybeCloseSettings(); }
    else if (v === "whatsnew" || v === "options") { e.preventDefault(); showView("main"); }
  } else if (e.key === "Enter" && currentView() === "settings" && !state.busy
             && e.target.tagName !== "TEXTAREA" && e.target.tagName !== "BUTTON") {
    // Enter commits the form from a FIELD. A focused button already means Enter, and swallowing
    // it here would make a keyboard user's Enter on a tab (or Browse, or Uninstall) silently
    // save-and-close instead of doing the thing they were pointing at.
    e.preventDefault(); saveSettings();
  }
});

// Quitting mid-operation: interrupted downloads resume on the next run, but a phase-2 commit
// should never be killed cold — confirm before closing while an op is running.
try {
  const appWindow = window.__TAURI__.window.getCurrentWindow();
  appWindow.onCloseRequested(async (ev) => {
    if (!state.busy) return;
    ev.preventDefault();
    const ok = await confirmDialog({
      title: t("cf.quitTitle"),
      text: t("cf.quitText"),
      confirm: t("cf.quitConfirm"),
    });
    if (ok) appWindow.destroy();
  });
} catch (e) { /* API shape differs — no guard, closing behaves as before */ }

$("ae-text").addEventListener("input", () => { setAeDirty(true); refreshAeHl(); });
$("ae-text").addEventListener("scroll", () => {
  const ta = $("ae-text");
  const hl = $("ae-hl");
  hl.scrollTop = ta.scrollTop;
  hl.scrollLeft = ta.scrollLeft;
});

// changelog links open in the default browser (webview must not navigate away); the launcher
// update banner renders the same markdown, so its links route the same way
for (const id of ["notes-body", "wn-launcher-body", "lu-notes"]) {
  $(id).addEventListener("click", (e) => {
    const a = e.target.closest("a[data-url]");
    if (!a) return;
    e.preventDefault();
    invoke("open_url", { url: a.dataset.url }).catch(() => {});
  });
}

// ---- boot ----
async function boot() {
  let settings = null;
  try { settings = await invoke("get_settings"); } catch (e) { /* defaults below */ }
  setLang(settings?.language || detectLang());
  applyAnimations(settings?.animations !== false); // absent/unknown = on
  state.hasToken = settings?.hasToken || false;
  try {
    const info = await invoke("launcher_info");
    state.launcherVersion = info.version;
    renderHeadVer();
    // set by the self-update that spawned us; applyCheck shows it once on the next check
    state.justUpdated = info.justUpdated;
  } catch (e) { /* cosmetic only — never worth failing boot over */ }
  applyStatic();
  setIdleStatus();
  renderPrimary();

  let firstRun = false;
  try {
    const gs = await invoke("game_dir_status");
    // setup only when nothing was ever chosen AND the exe isn't sitting next to a game
    firstRun = !gs.configured && !gs.clientVersion;
  } catch (e) { /* resolve failed вЂ” treat as first run */ firstRun = true; }
  return firstRun;
}

// Reveal the window only after the first frame is painted, to avoid the WebView2 white->black
// startup flash (the window is created hidden in tauri.conf.json).
requestAnimationFrame(() =>
  requestAnimationFrame(async () => {
    const firstRun = await boot();
    try {
      await window.__TAURI__.window.getCurrentWindow().show();
    } catch (e) {
      /* API shape differs вЂ” ignore; not fatal */
    }
    setTimeout(() => {
      $("loader").classList.add("hidden");
      // .rise animation plays whenever main first becomes visible (also after setup)
      $("view-main").classList.add("revealed");
      if (firstRun) {
        showView("setup");
      } else {
        doCheck(); // auto-check on launch
      }
      pollGame(); // start tracking the game
      setInterval(pollGame, 3000);
    }, 500);
  })
);
