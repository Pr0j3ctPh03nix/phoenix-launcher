const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

// Internal knob: the Advanced settings block (source repo / access token). Off = not rendered at
// all; the baked-in defaults apply. Flip to true for maintainer builds.
const SHOW_ADVANCED = false;

const state = {
  busy: false,
  lastCheck: null,     // last CheckView
  primaryMode: "check", // "check" | "apply" | "play" | "updateLauncher"
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
  fileEls: new Map(),  // dest -> its <li> in the managed-files list (keyed for live dl bars)
  settingsSnap: null,  // settings-form snapshot at open, for the discard-changes guard
  settingsLoaded: null, // {repo, game} as loaded — a save that changes them re-checks
  settingsTab: "general", // remembered for the session: a repeat visit lands where the user was
  tokenClear: false,   // "Clear" was pressed: the saved token is removed on save
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
const VIEWS = ["main", "setup", "settings", "options", "autoexec", "whatsnew"];
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

// ---- stop: an op's own offer to be interrupted ----
// Only a flow that can actually honour a stop puts one up (`offerStop`) — a Stop button the
// backend would ignore is worse than none. Today that is the game-files verify: minutes of hashing
// on the main view, writing nothing, with no way out but waiting for it. While the offer stands
// the PRIMARY becomes that Stop (see renderPrimary), and Escape fires the same offer.
// `asked` latches the request so late progress ticks can't paint over "Stopping…" and a second
// press can't fire a second cancel.
let stopOp = null;
let stopAsked = false;
function offerStop(fn) { stopOp = fn; stopAsked = false; renderPrimary(); }
function clearStop() { stopOp = null; stopAsked = false; renderPrimary(); }
function fireStop() {
  if (!stopOp || stopAsked) return;
  stopAsked = true;
  renderPrimary();
  stopOp();
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
        ? t("detail.localOk", { version: v.version })
        : t("detail.localChanged", { version: v.version, n: v.changes }),
    ];
  }
  if (v.changes === 0) {
    if (!v.installed) {
      // files all hash-match but no install state — the primary runs the no-op heal. Worded as
      // "repair", not "not installed": the list right below says every file is current, and
      // "Install" next to "all current" reads as a contradiction.
      return [t("status.repair"), "update", t("detail.repair", { version: v.version })];
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
    // An op that can be interrupted takes the primary over rather than adding a fourth control:
    // the status word right above already says "Working…", so a button repeating it was the least
    // useful thing on the screen — and a fourth button pushed the What's-new chip off the edge at
    // the minimum window size in Russian. Ghost, not gold: stopping is a way out, not the
    // recommended move. Same shape the download dialog's run stage has always had.
    p.textContent = stopOp ? t("btn.stop") : t("status.working");
    p.disabled = !stopOp || stopAsked; // pressed: it stays legible but can't fire twice
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
      apply: heal ? "btn.repair" : state.lastCheck?.installed ? "btn.update" : "btn.install",
    }[state.primaryMode];
    p.textContent = t(label);
    p.disabled = false;
    c.disabled = false;
  }
  // set from ONE place, outside the branches: a ghost left behind by a finished op would paint
  // the next Play/Install in the wrong weight entirely
  p.classList.toggle("ghost", state.busy && !!stopOp);
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
  $("files-count").textContent = !v.files.length
    ? "" // an empty (cleared) list isn't "all current" — it's not read yet
    : v.changes === 0 ? t("files.allCurrent") : t("files.toChange", { n: v.changes });
}

function applyCheck(v) {
  state.lastCheck = v;
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
    detail = t("detail.justUpdated", { version: state.launcherVersion }) + " · " + detail;
    state.justUpdated = false;
  }
  setStatus(word, kind, detail);

  renderFiles(v);

  const pl = $("game-path");
  pl.textContent = v.gameDir;
  pl.title = v.gameDir;

  // always offered once checked: the history view serves older releases' notes even when the
  // latest release carries none (the backend is built for exactly that case)
  $("btn-whatsnew").classList.remove("hidden");
  $("btn-customize").classList.toggle("hidden", !(v.options && v.options.length));

  // a pending launcher update takes the primary: this build may not be able to read the current
  // manifest at all, so replacing it comes before installing anything described by that manifest
  state.primaryMode = state.launcherUpdate
    ? "updateLauncher"
    : v.primaryAction === "apply" ? "apply" : v.canPlay ? "play" : "check";
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
  $("status-detail").textContent += " · " + errText(cause);
}

async function doReplan() {
  try {
    applyCheck(await invoke("replan"));
  } catch (e) {
    onError(e);
  }
}

async function doApply() {
  const busy = acquireBusy();
  if (busy == null) return;
  setStatus(t("status.working"), "busy", t("detail.installing"));
  // the engine streams phase-1 progress as op-progress events; downloads run in parallel, so
  // ticks for different files interleave. Each file's own bar (keyed by dest in state.fileEls)
  // fills from its byte ticks. The header counts DESTS done, not the engine's unique-asset
  // current/total — dests are what the visible rows are, so the numbers always match the list.
  const dlTotal = state.lastCheck
    ? state.lastCheck.files.filter((f) => f.status === "update" || f.status === "install").length
    : 0;
  const doneDests = new Set();
  let unlisten = null;
  try {
    unlisten = await listen("op-progress", (ev) => {
      const p = ev.payload;
      if (p.op !== "install" || !p.item) return;
      const li = state.fileEls.get(p.item);
      if (!li) return;
      li.classList.add("dl");
      const fill = li.querySelector(".fbar-fill");
      const st = li.querySelector(".fstate");
      if (p.done) {
        doneDests.add(p.item);
        li.classList.add("done");
        if (fill) fill.style.width = "100%";
        st.className = "fstate ok";
        st.textContent = t("fstate.ok");
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
      setStatus(t("status.working"), "busy", t("detail.dl", { i: doneDests.size, n: dlTotal || p.total }));
    });
    // pinned to the release this button is DESCRIBING (state.lastCheck) — same rule as the
    // launcher self-update: what the button offers is what the button installs. A local (offline)
    // verdict carries no tag and never offers apply, so null only means "no prior check".
    const tag = state.lastCheck && !state.lastCheck.local && state.lastCheck.tag
      ? state.lastCheck.tag : null;
    await invoke("apply", { tag });
    // no network: apply refreshed the backend's manifest cache from the release it installed
    await doReplan();
  } catch (e) {
    onError(e);
    // reset half-filled bars / "N MB" states to the last known plan (the status line keeps
    // showing the error — renderFiles doesn't touch it)
    if (state.lastCheck) renderFiles(state.lastCheck);
  } finally {
    if (unlisten) unlisten();
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
  if (stopOp) { fireStop(); return; } // while an op offers a stop, that IS the primary
  if (state.busy) return;
  if (state.primaryMode === "apply") doApply();
  else if (state.primaryMode === "play") doPlay();
  else if (state.primaryMode === "updateLauncher") doLauncherUpdate();
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
  renderLaunchFlags();
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
  perFile: null,   // Map dest -> bytesDone (ticks fan out per dest; clamp the sum to `bytes`)
  sum: 0,          // running total of perFile's values, maintained by delta (never re-summed)
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
  for (const s of ["gd-plan", "gd-confirm", "gd-run", "gd-err"])
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

// Fresh download: pick a DESTINATION, plan against it, confirm the numbers, run.
// The picker always runs — the download never silently reuses the configured game folder — and the
// files land directly in whatever is picked (no subfolder is invented), which is why the title and
// the confirm line both name the exact path. `game_install` then adopts that folder as the game
// dir backend-side, so a fresh download ends pointed at itself.
async function startGameDownload(origin) {
  if (state.busy) return;
  let dir = null;
  try {
    dir = await invoke("browse_folder", { title: t("dlg.pickTarget") });
  } catch (e) { /* dialog failed — stay put */ }
  if (!dir) return;
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
  gd.bytes = plan.bytes;
  gd.files = plan.files;
  $("gd-summary").textContent = t("gd.confirm", {
    gb: (plan.bytes / GB).toFixed(1), n: plan.files, dir,
  });
  // refuse up front what the backend would refuse a click later — same margin (512 MB)
  const short = plan.freeBytes != null && plan.freeBytes < plan.bytes + 512 * 1024 * 1024;
  $("gd-space").hidden = !short;
  if (short) {
    $("gd-space").textContent = t("gd.noSpace", {
      need: (plan.bytes / GB).toFixed(1), free: (plan.freeBytes / GB).toFixed(1),
    });
  }
  $("btn-gd-go").disabled = short;
  gdStage("confirm");
}

// Repair: verify already confirmed via dialog — straight to the run stage.
async function startGameRepair(v) {
  gd.mode = "repair";
  gd.origin = null;
  gd.bytes = v.damagedBytes;
  gd.files = v.damaged.length;
  $("gd-title").textContent = t("gd.repairTitle");
  $("gd-modal").classList.remove("hidden");
  gdRun();
}

function onGdProgress(ev) {
  const p = ev.payload;
  if (p.op === "game") {
    if (p.bytesTotal == null) {
      // plan/hash phase: no bytes, just the file counter
      $("gd-line1").textContent = t("gd.checking", { i: p.current, n: p.total });
      $("gd-line2").textContent = p.item || "";
    } else {
      // running total, updated by DELTA. Re-summing the map per tick was O(files) inside an
      // O(ticks) stream — ~60k ticks against a map growing to 4,635 entries is a few hundred
      // million iterations of pure waste on the UI thread during the download.
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
      ? await invoke("game_repair")
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
    state.gameDamaged = 0;
    renderPrimary();
    setStatus(t("status.gvOk"), "ok", t("gv.repaired", { n: result.written }));
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
async function doGameVerify() {
  const busy = acquireBusy();
  if (busy == null) return;
  setStatus(t("status.working"), "busy", t("gv.start"));
  // Reading a whole install is minutes of work that writes nothing — the one op where quitting
  // costs the user nothing but the hashing already done (and the memo keeps even that).
  offerStop(() => {
    // the hash workers stop BETWEEN files, so a multi-GB VPK in flight still has to finish; say
    // that rather than leaving a dead button over an unchanged line
    setStatus(t("status.working"), "busy", t("gv.stopping"));
    invoke("game_cancel").catch(() => {});
  });
  let unlisten = null;
  let damagedResult = null;
  try {
    unlisten = await listen("op-progress", (ev) => {
      const p = ev.payload;
      if (p.op !== "verify" || stopAsked) return; // ticks in flight must not repaint "Stopping…"
      setStatus(t("status.working"), "busy", t("gv.progress", { i: p.current, n: p.total }));
    });
    const r = await invoke("game_verify");
    if (r.damaged.length === 0) {
      // its own word, NOT "Up to date": that one describes the Phoenix files, and the list right
      // below still shows whatever the shim's own state is. Two different subjects were sharing
      // one status line, so "Up to date" could sit above "2 to change".
      state.gameDamaged = 0; // the files are provably fine — clear any earlier declined repair
      setStatus(t("status.gvOk"), "ok", t("gv.ok", { n: r.ok, version: r.version }));
    } else if (r.foreignBuild) {
      // NOT damage: this folder holds a different build, so a "repair" would overwrite a working
      // unrelated install. Repairing is still allowed — it is the user's folder, and the project
      // does not gate on build elsewhere either — but it is named for what it is, in the error
      // colour, and the confirm below spells out the consequence instead of saying "repair".
      // This verdict SUPERSEDES an older damaged one: the verify just concluded nothing here is
      // broken, so a damaged count from a previous run must not keep blocking Play.
      state.gameDamaged = 0;
      setStatus(t("status.gvForeign"), "error", t("gv.foreign", { version: r.version }));
      damagedResult = r;
    } else {
      setStatus(t("status.gvDamaged"), "update", t("gv.damaged", { n: r.damaged.length, total: r.total }));
      damagedResult = r;
    }
  } catch (e) {
    // The user asked for this stop, so it is not an error — but it is not a verdict either. The
    // neutral colour and its own wording keep it from reading as "checked, all fine": the files
    // nobody got to are exactly the ones this run can say nothing about. An earlier declined
    // repair still stands (`state.gameDamaged` is untouched), so Play stays blocked if it was.
    if (e && e.kind === "cancelled") setStatus(t("status.gvStopped"), "idle", t("gv.stopped"));
    else onError(e);
  } finally {
    // unlisten BEFORE clearing the latch: a tick emitted just before the workers joined can still
    // be in flight, and it would land on the "Stopped" line as "Verifying 1342/4635" — a dead
    // progress line under a finished op. While the listener lives, `stopAsked` is what holds it.
    if (unlisten) unlisten();
    clearStop();
    releaseBusy(busy);
  }
  if (damagedResult) {
    // a foreign build is not a repair — it is an overwrite of a working installation, so the
    // dialog says so and its confirm button is worded as the destructive act it is
    const foreign = damagedResult.foreignBuild;
    const ok = await confirmDialog({
      title: foreign ? t("cf.foreignTitle") : t("cf.repairTitle"),
      text: foreign
        ? t("cf.foreignText", { n: damagedResult.damaged.length, version: damagedResult.version })
        : t("cf.repairText", { n: damagedResult.damaged.length }),
      confirm: foreign ? t("cf.foreignConfirm") : t("btn.repair"),
      danger: foreign, // a repair restores; an overwrite destroys a working install
    });
    if (ok) {
      startGameRepair(damagedResult);
    } else if (!foreign) {
      // Declining leaves the game files damaged. Remember it: Play must not go on offering to
      // launch a client this very check just reported as broken, and the status line has to keep
      // saying so rather than being quietly replaced by the shim's verdict.
      state.gameDamaged = damagedResult.damaged.length;
      renderPrimary();
    }
    // Declining the FOREIGN overwrite is different: verify's own verdict is that nothing is
    // damaged — the folder holds another build, and the whole point of the distinction is that
    // it works. Blocking Play here painted a working install as broken with no way out short of
    // overwriting it (re-verifying re-armed the flag every time). No install gate: the status
    // line keeps saying "different build", and launching it stays the user's call.
  }
}

// ---- confirm modal (destructive actions: uninstall, discard autoexec changes) ----
let cfResolve = null;
// `danger` paints the confirm terracotta instead of gold — reserved for the two irreversible
// acts (uninstall, overwriting a foreign build). Kept rare on purpose: a red button that shows up
// for "discard my edits" stops meaning anything.
function confirmDialog({ title, text, confirm, danger }) {
  $("cf-title").textContent = title;
  $("cf-text").textContent = text;
  $("btn-cf-ok").textContent = confirm;
  $("btn-cf-ok").classList.toggle("danger", !!danger);
  $("cf-modal").classList.remove("hidden");
  $("btn-cf-ok").focus(); // keyboard path: Enter confirms, Escape cancels, Tab reaches Cancel
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
$("btn-verify").addEventListener("click", async () => {
  if (currentView() === "settings" && !(await maybeCloseSettings())) return;
  doGameVerify();
});
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
$("btn-cf-ok").addEventListener("click", () => settleConfirm(true));
$("btn-cf-cancel").addEventListener("click", () => settleConfirm(false));
wireSeg($("seg-lang"), (l) => switchLang(l));
wireSeg($("seg-renderer"));

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
    const v = currentView();
    // main's running op backs out the same way a dialog does — the Stop button is the visible
    // route, Escape is the one a keyboard reaches for first
    if (v === "main" && stopOp) { e.preventDefault(); fireStop(); }
    else if (v === "autoexec") { e.preventDefault(); maybeCloseAutoexec(); }
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
for (const id of ["notes-body", "lu-notes"]) {
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
  state.hasToken = settings?.hasToken || false;
  try {
    const info = await invoke("launcher_info");
    state.launcherVersion = info.version;
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
