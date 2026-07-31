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
};

// ---- markdown-lite: the notes are trusted (our own manifest) but escape anyway, then apply the
// changelog subset: headings, bullet + ordered lists, ``` fences, **bold**, *italic*, `code`,
// [links](https://…). No raw HTML from the source ever reaches innerHTML; links go through the
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
    // single-* italics only — _underscores_ stay literal (file_names are common in changelogs)
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
  if (state.busy) {
    p.textContent = t("status.working");
    p.disabled = true;
    c.disabled = true;
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
  u.disabled = state.busy;
  $("btn-customize").disabled = state.busy;
  $("btn-settings").disabled = state.busy;
}

function applyCheck(v) {
  state.lastCheck = v;
  const [word, kind, detail] = statusFor(v);
  setStatus(word, kind, detail);

  const ul = $("files");
  ul.innerHTML = "";
  for (const f of v.files) {
    const li = document.createElement("li");
    const path = document.createElement("span");
    path.className = "fpath";
    path.textContent = f.dest;
    const st = document.createElement("span");
    st.className = "fstate " + f.status;
    st.textContent = t("fstate." + f.status);
    li.append(path, st);
    ul.append(li);
  }
  $("files-empty").style.display = v.files.length ? "none" : "flex";
  $("files-count").textContent = v.changes === 0 ? t("files.allCurrent") : t("files.toChange", { n: v.changes });

  const pl = $("game-path");
  pl.textContent = v.gameDir;
  pl.title = v.gameDir;

  $("btn-whatsnew").classList.toggle("hidden", !v.notes);
  $("btn-customize").classList.toggle("hidden", !(v.options && v.options.length));

  state.primaryMode = v.primaryAction === "apply" ? "apply" : v.canPlay ? "play" : "check";
  renderPrimary();
}

function onError(e) {
  setStatus(t("status.error"), "error", String(e));
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
  try {
    await invoke("apply");
    await doCheck(); // refresh -> up to date, Play unlocks
  } catch (e) {
    onError(e);
    state.busy = false; renderPrimary();
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
    setSettingsMsg(String(e));
  }
}

async function browseInto(input) {
  try {
    const dir = await invoke("browse_folder");
    if (dir) input.value = dir;
  } catch (e) {
    setSettingsMsg(String(e));
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
    setSetupMsg(String(e));
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
    setSetupMsg(String(e));
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
    // scan failed outright — show empty results rather than a dead modal
  }
  if (state.afUnlisten) { state.afUnlisten(); state.afUnlisten = null; }
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
  // the running scan returns what it found so far; results stage follows from runAutofind()
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

// Source cfg highlight: `<command> <value…>` per line, `//` comments, "quoted" values.
function hlAutoexec(text) {
  const lines = text.split(/\r?\n/).map((line) => {
    const ci = line.indexOf("//");
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
    setAeMsg(String(e));
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
    setAeMsg(String(e));
  }
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
  if (!v?.notes) return;
  const seq = ++wnSeq;
  const body = $("notes-body");
  // instant first paint: the current release's notes, history swaps in when it arrives
  body.innerHTML = "";
  body.append(notesSection(v.version, v.notes));
  const loading = document.createElement("div");
  loading.className = "hint notes-loading";
  loading.textContent = t("wn.loading");
  body.append(loading);
  body.parentElement.scrollTop = 0;
  showView("whatsnew");
  try {
    const all = await invoke("release_notes"); // cached backend-side (memory + disk), instant after first fetch
    if (seq !== wnSeq) return;
    if (all.length) {
      body.innerHTML = "";
      for (const e of all) body.append(notesSection(e.version, e.notes));
    } else {
      loading.remove(); // no history found — keep the current release's notes
    }
  } catch (e) {
    if (seq === wnSeq) loading.remove(); // offline etc. — current release's notes stay up
  }
}

// ---- wire ----
$("btn-primary").addEventListener("click", onPrimary);
$("btn-check").addEventListener("click", () => !state.busy && doCheck());
$("btn-uninstall").addEventListener("click", () => !state.busy && doUninstall());
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
$("btn-ae-close").addEventListener("click", () => showView("settings"));
wireSeg($("seg-lang"), (l) => switchLang(l));
wireSeg($("seg-renderer"));

// Escape backs out; Enter in settings commits the save.
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (!$("af-modal").classList.contains("hidden")) {
      if (!$("af-run").classList.contains("hidden")) cancelAutofind();
      else closeAutofind();
      return;
    }
    const v = currentView();
    if (v === "autoexec") { e.preventDefault(); showView("settings"); }
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
  } catch (e) { /* resolve failed — treat as first run */ firstRun = true; }
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
      /* API shape differs — ignore; not fatal */
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
    }, 500);
  })
);
