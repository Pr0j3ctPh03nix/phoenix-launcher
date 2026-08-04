const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

// Internal knob: the Advanced settings block (source repo / access token). Off = not rendered at
// all; the baked-in defaults apply. Flip to true for maintainer builds.
const SHOW_ADVANCED = false;

const state = {
  busy: false,
  lastCheck: null,     // last CheckView
  primaryMode: "check", // "check" | "apply" | "play"
  hasToken: false,
  renderer: "dx11",
  afTarget: null,      // "setup" | "settings" — where an autofind pick lands
  afUnlisten: null,
  aeDirty: false,
  gameRunning: false,  // polled — the game is currently running
  fileEls: new Map(),  // dest -> its <li> in the managed-files list (keyed for live dl bars)
};

// ---- markdown-lite: the notes are trusted (our own manifest) but escape anyway, then apply the
// changelog subset: headings, bullet + ordered lists, ``` fences, **bold**, *italic*, `code`,
// [links](https://вЂ¦). No raw HTML from the source ever reaches innerHTML; links go through the
// open_url command (http/https only). ----
function escHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
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
  let html = "";
  let items = null;    // open list's item texts
  let listTag = "ul";  // tag of the open list
  let para = null;     // open paragraph text
  let fence = null;    // open ``` fence's raw lines
  const flushList = () => { if (items) { html += `<${listTag}>` + items.map((t) => `<li>${inline(t)}</li>`).join("") + `</${listTag}>`; items = null; } };
  const flushPara = () => { if (para != null) { html += `<p>${inline(para)}</p>`; para = null; } };
  const flushAll = () => { flushList(); flushPara(); };
  const openList = (tag) => { if (!items || listTag !== tag) { flushList(); items = []; listTag = tag; } };
  for (const raw of md.split(/\r?\n/)) {
    if (fence !== null) {
      if (/^```/.test(raw.trim())) { html += `<pre><code>${escHtml(fence.join("\n"))}</code></pre>`; fence = null; }
      else fence.push(raw);
      continue;
    }
    const line = raw.trim();
    if (/^```/.test(line)) { flushAll(); fence = []; continue; }
    if (!line) { flushAll(); continue; }
    let m;
    if ((m = line.match(/^#{1,6}\s+(.*)$/))) { flushAll(); html += `<h4>${inline(m[1])}</h4>`; }
    else if ((m = line.match(/^[-*]\s+(.*)$/))) { flushPara(); openList("ul"); items.push(m[1]); }
    else if ((m = line.match(/^\d+[.)]\s+(.*)$/))) { flushPara(); openList("ol"); items.push(m[1]); }
    // lazy continuation of the current bullet / paragraph (wrapped line)
    else if (items) items[items.length - 1] += " " + line;
    else para = para == null ? line : para + " " + line;
  }
  if (fence !== null) html += `<pre><code>${escHtml(fence.join("\n"))}</code></pre>`; // unclosed fence
  flushAll();
  return html;
}

// ---- views ----
const VIEWS = ["main", "setup", "settings", "options", "autoexec", "whatsnew"];
function showView(name) {
  for (const v of VIEWS) $("view-" + v).classList.toggle("hidden", v !== name);
}
function currentView() {
  return VIEWS.find((v) => !$("view-" + v).classList.contains("hidden"));
}

// ---- status ----
function setStatus(word, kind, detail) {
  $("status").dataset.kind = kind || "idle";
  $("status-word").textContent = word;
  $("status-detail").textContent = detail || "";
}

function setIdleStatus() {
  setStatus(t("status.notChecked"), "idle", t("status.checkHint"));
}

function statusFor(v) {
  if (v.changes === 0) {
    if (!v.installed) {
      // files all hash-match but no install state вЂ” Apply runs the no-op heal (rewrites state)
      return [t("status.notInstalled"), "update", t("detail.okMeta", { version: v.version })];
    }
    return [t("status.upToDate"), "ok", t("detail.okMeta", { version: v.version })];
  }
  return [
    v.installed ? t("status.updateAvail") : t("status.notInstalled"),
    "update",
    t("detail.changes", { version: v.version, n: v.changes }),
  ];
}

// ---- primary / buttons ----
function renderPrimary() {
  const p = $("btn-primary");
  const c = $("btn-check");
  // busy wins over "in game": a running op keeps everything locked either way
  if (state.busy) {
    p.textContent = t("status.working");
    p.disabled = true;
    c.disabled = true;
  } else if (state.gameRunning && state.primaryMode !== "check") {
    // the game is up: nothing mutating is offered (the backend interlock backs this up);
    // check stays available — it's read-only. In "check" mode the primary IS the check
    // button, so it falls through to the normal branch and stays clickable.
    p.textContent = t("btn.ingame");
    p.disabled = true;
    c.disabled = false;
  } else {
    const label = { check: "btn.check", play: "btn.play", apply: state.lastCheck?.installed ? "btn.update" : "btn.install" }[state.primaryMode];
    p.textContent = t(label);
    p.disabled = false;
    c.disabled = false;
  }
  // the header refresh icon appears whenever the primary is something else
  c.classList.toggle("hidden", state.primaryMode === "check");

  const u = $("btn-uninstall");
  u.classList.toggle("hidden", !state.lastCheck?.canUninstall);
  u.disabled = state.busy || state.gameRunning;
  $("btn-customize").disabled = state.busy;
  $("btn-settings").disabled = state.busy;
}

// Rebuild the managed-files list from a CheckView. Separate from applyCheck so a failed apply
// can reset half-filled bars / "N MB" states without touching the (error) status line.
function renderFiles(v) {
  const ul = $("files");
  ul.innerHTML = "";
  state.fileEls.clear();
  for (const f of v.files) {
    const li = document.createElement("li");
    li.dataset.dest = f.dest;
    const path = document.createElement("span");
    path.className = "fpath";
    path.textContent = f.dest;
    const st = document.createElement("span");
    st.className = "fstate " + f.status;
    st.textContent = t("fstate." + f.status);
    // files that will be fetched (update/install) carry a hairline bar, revealed + filled live
    // from op-progress ticks during apply (downloads run in parallel — one bar each)
    const bar = document.createElement("span");
    bar.className = "fbar";
    bar.innerHTML = '<span class="fbar-fill"></span>';
    li.append(path, st, bar);
    ul.append(li);
    state.fileEls.set(f.dest, li);
  }
  $("files-empty").style.display = v.files.length ? "none" : "flex";
  $("files-count").textContent = v.changes === 0 ? t("files.allCurrent") : t("files.toChange", { n: v.changes });
}

function applyCheck(v) {
  state.lastCheck = v;
  // a check completing while the game runs must not overwrite the "in game" status
  const [word, kind, detail] = state.gameRunning
    ? [t("status.ingame"), "ok", t("detail.ingame")]
    : statusFor(v);
  setStatus(word, kind, detail);

  renderFiles(v);

  const pl = $("game-path");
  pl.textContent = v.gameDir;
  pl.title = v.gameDir;

  // always offered once checked: the history view serves older releases' notes even when the
  // latest release carries none (the backend is built for exactly that case)
  $("btn-whatsnew").classList.remove("hidden");
  $("btn-customize").classList.toggle("hidden", !(v.options && v.options.length));

  state.primaryMode = v.primaryAction === "apply" ? "apply" : v.canPlay ? "play" : "check";
  renderPrimary();
}

// Command failures arrive as {kind, message} envelopes (CmdError); tolerate bare strings too.
// `kind` is for reacting (later: token prompts on "auth", update nudges on "tooOld") вЂ” for now
// everything displays the message.
function errText(e) {
  return (e && typeof e === "object" && "message" in e) ? e.message : String(e);
}

function onError(e) {
  setStatus(t("status.error"), "error", errText(e));
}

// ---- actions ----
async function doCheck() {
  state.busy = true; renderPrimary();
  setStatus(t("status.working"), "busy", t("detail.reading"));
  try {
    applyCheck(await invoke("check"));
  } catch (e) {
    onError(e);
  } finally {
    state.busy = false; renderPrimary();
  }
}

async function doReplan() {
  try {
    applyCheck(await invoke("replan"));
  } catch (e) {
    onError(e);
  }
}

async function doApply() {
  state.busy = true; renderPrimary();
  setStatus(t("status.working"), "busy", t("detail.installing"));
  // the engine streams phase-1 progress as op-progress events; downloads run in parallel, so
  // ticks for different files interleave. The header shows the completed/total count; each file's
  // own bar (keyed by dest in state.fileEls) fills from its byte ticks.
  let unlisten = null;
  try {
    unlisten = await listen("op-progress", (ev) => {
      const p = ev.payload;
      if (p.op !== "install") return;
      setStatus(t("status.working"), "busy", t("detail.dl", { i: p.current, n: p.total }));
      if (!p.item) return;
      const li = state.fileEls.get(p.item);
      if (!li) return;
      li.classList.add("dl");
      const fill = li.querySelector(".fbar-fill");
      const st = li.querySelector(".fstate");
      if (p.done) {
        li.classList.add("done");
        if (fill) fill.style.width = "100%";
        st.className = "fstate ok";
        st.textContent = t("fstate.ok");
      } else if (p.bytesTotal) {
        const pct = Math.min(100, (p.bytesDone / p.bytesTotal) * 100);
        if (fill) fill.style.width = pct.toFixed(1) + "%";
        st.className = "fstate dl";
        st.textContent = `${(p.bytesDone / 1048576).toFixed(1)}/${(p.bytesTotal / 1048576).toFixed(1)} MB`;
      }
    });
    await invoke("apply");
    await doCheck(); // refresh -> up to date, Play unlocks
  } catch (e) {
    onError(e);
    // reset half-filled bars / "N MB" states to the last known plan (the status line keeps
    // showing the error — renderFiles doesn't touch it)
    if (state.lastCheck) renderFiles(state.lastCheck);
    state.busy = false; renderPrimary();
  } finally {
    if (unlisten) unlisten();
  }
}

async function doUninstall() {
  state.busy = true; renderPrimary();
  setStatus(t("status.working"), "busy", t("detail.reverting"));
  try {
    await invoke("uninstall");
    await doCheck();
  } catch (e) {
    onError(e);
    state.busy = false; renderPrimary();
  }
}

async function doPlay() {
  if (state.busy) return;
  state.busy = true; renderPrimary();
  try {
    await invoke("play");
    setStatus(t("status.launched"), "ok", t("detail.launched"));
  } catch (e) {
    onError(e);
  } finally {
    state.busy = false; renderPrimary();
  }
}

function onPrimary() {
  if (state.busy) return;
  if (state.primaryMode === "apply") doApply();
  else if (state.primaryMode === "play") doPlay();
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
  if (state.lastCheck) applyCheck(state.lastCheck);
  else { setIdleStatus(); renderPrimary(); }
  updateTokenPlaceholder();
  renderOptions();
}

async function switchLang(l) {
  setLang(l);
  setSeg($("seg-lang"), l);
  rerenderDynamic();
  try { await invoke("set_language", { language: l }); } catch (e) { /* non-fatal */ }
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
function setSettingsMsg(text) {
  const m = $("settings-msg");
  if (text) { m.textContent = text; m.hidden = false; } else { m.hidden = true; }
}

function updateTokenPlaceholder() {
  $("in-token").placeholder = state.hasToken ? t("ph.tokenSaved") : t("ph.tokenEmpty");
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
  state.renderer = s.renderer || "dx11";
  updateTokenPlaceholder();
  setSeg($("seg-renderer"), state.renderer);
  setSeg($("seg-lang"), LANG);
  $("advanced").classList.toggle("hidden", !SHOW_ADVANCED);
  $("advanced").open = false;
  setSettingsMsg(null);
  showView("settings");
}

async function saveSettings() {
  try {
    await invoke("save_settings", {
      sourceRepo: $("in-repo").value,
      gameDir: $("in-game").value,
      token: $("in-token").value,
      language: LANG,
      launchExtra: $("in-launch").value,
      renderer: segValue($("seg-renderer")) || "dx11",
    });
    showView("main");
    setStatus(t("status.saved"), "ok", "");
  } catch (e) {
    setSettingsMsg(errText(e));
  }
}

async function browseInto(input) {
  try {
    const dir = await invoke("browse_folder");
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
    setSetupMsg(errText(e));
    return;
  }
  closeAutofind();
  showView("main");
  setIdleStatus();
  doCheck();
}

async function setupBrowse() {
  try {
    const dir = await invoke("browse_folder");
    if (dir) adoptGameDir(dir); // any folder is accepted
  } catch (e) {
    setSetupMsg(errText(e));
  }
}

// ---- autofind ----
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
  afStage("run");
  $("af-count").textContent = t("af.scanning");
  $("af-current").textContent = "";
  state.afUnlisten = await listen("autofind-progress", (ev) => {
    const p = ev.payload;
    $("af-count").textContent = t("af.scanned", { n: p.scanned });
    $("af-current").textContent = p.current;
  });
  let found = [];
  try {
    found = await invoke("autofind_start");
  } catch (e) {
    // scan failed outright вЂ” show empty results rather than a dead modal
  }
  if (state.afUnlisten) { state.afUnlisten(); state.afUnlisten = null; }
  // the modal was closed mid-scan (Escape) вЂ” discard the results instead of staging them
  // under a hidden modal
  if ($("af-modal").classList.contains("hidden")) return;
  renderCandidates(found);
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

// ---- confirm modal (destructive actions: uninstall, discard autoexec changes) ----
let cfResolve = null;
function confirmDialog({ title, text, confirm }) {
  $("cf-title").textContent = title;
  $("cf-text").textContent = text;
  $("btn-cf-ok").textContent = confirm;
  $("cf-modal").classList.remove("hidden");
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

function queueSelection(id, value) {
  selChain = selChain.then(async () => {
    try {
      await invoke("set_selection", { id, value });
      await doReplan(); // cached manifest, no network
    } catch (e) {
      onError(e);
    }
  });
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
        o.value = !(o.value === true);
        sw.classList.toggle("on", o.value);
        sw.setAttribute("aria-checked", String(o.value));
        queueSelection(o.id, o.value);
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
          o.value = v.id;
          for (const r of list.children) r.classList.toggle("active", r === row);
          queueSelection(o.id, v.id);
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
  try {
    $("ae-text").value = await invoke("read_autoexec");
  } catch (e) {
    $("ae-text").value = "";
    setAeMsg(errText(e));
  }
  refreshAeHl();
  setAeDirty(false);
  setAeMsg(null);
  showView("autoexec");
}

async function saveAutoexec() {
  try {
    await invoke("save_autoexec", { content: $("ae-text").value });
    setAeDirty(false);
    setAeMsg(null);
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
// One version section: mono version line + rendered notes.
function notesSection(version, notes) {
  const frag = document.createDocumentFragment();
  const h = document.createElement("div");
  h.className = "whatsnew-version";
  h.textContent = version;
  const n = document.createElement("div");
  n.className = "notes";
  n.innerHTML = renderNotes(notes);
  frag.append(h, n);
  return frag;
}

let wnSeq = 0; // drops a stale history fetch when the view was reopened meanwhile

async function openWhatsNew() {
  const v = state.lastCheck;
  if (!v) return;
  const seq = ++wnSeq;
  const body = $("notes-body");
  // instant first paint: the current release's notes (if any), history swaps in when it arrives
  body.innerHTML = "";
  if (v.notes) body.append(notesSection(v.version, v.notes));
  const loading = document.createElement("div");
  loading.className = "hint notes-loading";
  loading.textContent = t("wn.loading");
  body.append(loading);
  body.parentElement.scrollTop = 0;
  showView("whatsnew");
  try {
    const all = await invoke("release_notes"); // cached backend-side (memory + disk), instant after first fetch
    if (seq !== wnSeq) return;
    body.innerHTML = "";
    if (all.length) {
      for (const e of all) body.append(notesSection(e.version, e.notes));
    } else if (v.notes) {
      body.append(notesSection(v.version, v.notes)); // no history вЂ” keep the current release's notes
    } else {
      const none = document.createElement("div");
      none.className = "hint notes-loading";
      none.textContent = t("wn.none");
      body.append(none);
    }
  } catch (e) {
    // offline etc. вЂ” the current release's notes (if any) stay up; say why the rest is missing
    if (seq === wnSeq) loading.textContent = errText(e);
  }
}

// ---- wire ----
$("btn-primary").addEventListener("click", onPrimary);
$("btn-check").addEventListener("click", () => !state.busy && doCheck());
$("btn-uninstall").addEventListener("click", async () => {
  if (state.busy) return;
  const ok = await confirmDialog({
    title: t("cf.uninstallTitle"),
    text: t("cf.uninstallText"),
    confirm: t("cf.uninstallConfirm"),
  });
  if (ok) doUninstall();
});
$("btn-settings").addEventListener("click", () => !state.busy && openSettings());
$("btn-whatsnew").addEventListener("click", () => !state.busy && openWhatsNew());
$("btn-customize").addEventListener("click", () => { if (!state.busy) { renderOptions(); showView("options"); } });
$("btn-options-back").addEventListener("click", () => showView("main"));
$("btn-whatsnew-back").addEventListener("click", () => showView("main"));
$("btn-save").addEventListener("click", saveSettings);
$("btn-back").addEventListener("click", () => showView("main"));
$("btn-browse").addEventListener("click", () => browseInto($("in-game")));
$("btn-autofind").addEventListener("click", () => openAutofind("settings"));
$("btn-setup-browse").addEventListener("click", setupBrowse);
$("btn-setup-autofind").addEventListener("click", () => openAutofind("setup"));
$("btn-af-go").addEventListener("click", runAutofind);
$("btn-af-close").addEventListener("click", closeAutofind);
$("btn-af-cancel").addEventListener("click", cancelAutofind);
$("btn-af-done").addEventListener("click", closeAutofind);
$("btn-autoexec").addEventListener("click", openAutoexec);
$("btn-ae-save").addEventListener("click", saveAutoexec);
$("btn-ae-close").addEventListener("click", maybeCloseAutoexec);
$("btn-cf-ok").addEventListener("click", () => settleConfirm(true));
$("btn-cf-cancel").addEventListener("click", () => settleConfirm(false));
wireSeg($("seg-lang"), (l) => switchLang(l));
wireSeg($("seg-renderer"));

// Escape backs out (topmost layer first); Enter in settings commits the save.
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (!$("cf-modal").classList.contains("hidden")) { settleConfirm(false); return; }
    if (!$("af-modal").classList.contains("hidden")) {
      if (!$("af-run").classList.contains("hidden")) cancelAutofind();
      closeAutofind();
      return;
    }
    const v = currentView();
    if (v === "autoexec") { e.preventDefault(); maybeCloseAutoexec(); }
    else if (v === "settings" || v === "whatsnew" || v === "options") { e.preventDefault(); showView("main"); }
  } else if (e.key === "Enter" && currentView() === "settings" && !state.busy && e.target.tagName !== "TEXTAREA") {
    e.preventDefault(); saveSettings();
  }
});

$("ae-text").addEventListener("input", () => { setAeDirty(true); refreshAeHl(); });
$("ae-text").addEventListener("scroll", () => {
  const ta = $("ae-text");
  const hl = $("ae-hl");
  hl.scrollTop = ta.scrollTop;
  hl.scrollLeft = ta.scrollLeft;
});

// changelog links open in the default browser (webview must not navigate away)
$("notes-body").addEventListener("click", (e) => {
  const a = e.target.closest("a[data-url]");
  if (!a) return;
  e.preventDefault();
  invoke("open_url", { url: a.dataset.url }).catch(() => {});
});

// ---- boot ----
async function boot() {
  let settings = null;
  try { settings = await invoke("get_settings"); } catch (e) { /* defaults below */ }
  setLang(settings?.language || detectLang());
  state.hasToken = settings?.hasToken || false;
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
