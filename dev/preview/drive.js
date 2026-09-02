// Preview-only director (loaded by dev/shoot.sh, never shipped): location.hash picks the screen,
// so a headless screenshot can reach views that normally need clicks. Referencing main.js's
// top-level functions works because both are classic scripts sharing one global scope.
//
// Add a screen by adding a branch here and passing its hash to dev/shoot.sh.
window.addEventListener("load", () => {
  setTimeout(async () => {
    const h = decodeURIComponent((location.hash || "#main").slice(1));
    document.getElementById("loader").style.display = "none";
    // boot()'s reveal rides a double rAF that headless only services when the capture forces a
    // frame — by then it would stomp the painted status with the idle one. Force the class,
    // neutralize setIdleStatus, and run the check boot would have run.
    document.getElementById("view-main").classList.add("revealed");
    // ...and then take the animation out of the equation entirely. Forcing `.revealed` only
    // STARTS the staggered reveal; the capture still races it, and a frame taken mid-stagger has
    // every element at opacity 0 — main is nothing but `.rise` elements, so the PNG comes out
    // blank while the DOM is perfectly correct. That flake cost real debugging time twice. These
    // screenshots are a LAYOUT check, so pin the end state and let the animation be irrelevant.
    const settle = document.createElement("style");
    settle.textContent =
      ".rise { opacity: 1 !important; transform: none !important; animation: none !important; }" +
      // ...and no TRANSITIONS either, for the same reason one step down. A capture can commit the
      // first frame of one, which paints the state the DOM has already left: a switch drawn lit
      // and knob-right over `aria-checked="false"`, on a screen whose whole point was that it is
      // off. The DOM was right both times — believe --dump-dom, not the PNG, and then remove the
      // transition from the equation like the reveal above.
      "*, *::before, *::after { transition: none !important; }";
    document.head.append(settle);
    setIdleStatus = () => {};
    // ?lang=ru — Russian labels are the long ones; worth a look before calling a layout done
    const lang = new URLSearchParams(location.search).get("lang");
    if (lang) await switchLang(lang);
    // boot() rides that same unserviced rAF, so nothing has asked which launcher this is — and
    // the What's-new launcher page marks the running build with it. Without this the pill that
    // says "current" is missing from every screenshot for no reason the DOM can explain.
    try { state.launcherVersion = (await invoke("launcher_info")).version; } catch (e) {}
    await doCheck();

    if (h.startsWith("settings")) {
      await openSettings();
      const tab = h.split(":")[1] || "general"; // settings:general | :launch | :files
      setSettingsTab(tab);
    } else if (h === "autoexec") {
      // the editor over the stub's cfg, which includes pinned-convar lines — the strikethrough
      // and the notice under the editor are the point of this screen
      await openAutoexec();
    } else if (h === "setup") {
      showView("setup");
    } else if (h === "options") {
      renderOptions();
      showView("options");
    } else if (h.startsWith("whatsnew")) {
      openWhatsNew();
      setWnTab(h.split(":")[1] || "phoenix"); // whatsnew:phoenix | :launcher
    } else if (h === "confirm:restore2") {
      // both statements apply: one file ticked, one left alone — the card that used to run them
      // together into "…(0 B). 1 file(s)" at the end of a line
      openFilesView({ ...GV, files: GV.files.filter((f) =>
        f.path === "game/dota/cfg/autoexec.cfg" || f.path === "game/core/pak01_dir.vpk") });
      gv.sel.clear();
      gv.sel.add("game/core/pak01_dir.vpk");
      gvRebuild();
      document.getElementById("btn-gv-restore").click();
    } else if (h === "confirm:restore") {
      // The reported card: nothing ticked, one file pinned. Its first statement is then about
      // NOTHING ("0 file(s) will be downloaded again (0 B)") and used to share a line with the one
      // that matters — which is why this screen exists.
      openFilesView({ ...GV, files: GV.files.filter((f) => f.path === "game/dota/cfg/autoexec.cfg") });
      gv.sel.clear();
      gvRebuild();
      document.getElementById("btn-gv-restore").click();
    } else if (h === "confirm:keep") {
      // the longest confirm copy in the app — the one that shows how a card's text uses its width
      confirmDialog({ title: t("cf.keepTitle"), text: t("cf.keepText", { n: 1 }),
        confirm: t("cf.keepConfirm"), cancel: t("cf.keepDiscard"), defaultCancel: true });
    } else if (h === "confirm") {
      document.getElementById("btn-uninstall").click(); // the real handler, danger flag and all
    } else if (h === "verify" || h === "verify:stopping") {
      // mid-verify: the stub's game_verify never resolves, so the run state holds still. The
      // progress lines are faked because the op-progress event has no stub to fire it. Note the
      // view underneath is settings — that is the point of the modal.
      await openSettings();
      setSettingsTab("files");
      doGameVerify("settings");
      document.getElementById("vf-line1").textContent = t("gv.progress", { i: 1342, n: 4635 });
      document.getElementById("vf-line2").textContent =
        t("gv.progressBytes", { item: "game/dota/pak01_dir.vpk", done: "240 MB", total: "340 MB" });
      if (h.endsWith("stopping")) document.getElementById("btn-vf-stop").click();
    } else if (h.startsWith("files")) {
      // the files view over the stub's mixed payload. `files:open` expands the modded folder so
      // the leaf rows (state + evidence) are visible; `files:extras` opens the extras page.
      const mode = h.split(":")[1];
      // `files:onlyextras` is the case that read as broken: nothing on screen is restorable, so
      // the primary must not sit greyed at "Keep 0" and the bulk controls must still work.
      openFilesView(mode === "onlyextras"
        ? { ...GV, files: GV.files.filter((f) => f.owner === "extra") }
        : GV);
      if (mode === "open") {
        for (const n of gv.root.kids.values()) n.open = true;
        const dota = [...gv.root.kids.values()][0];
        if (dota) for (const k of dota.kids.values()) k.open = true;
        gvRebuild();
      } else if (mode === "extras" || mode === "doomed") {
        gvGoPage("extra");
        for (const n of gv.root.kids.values()) n.open = true;
        const dota = [...gv.root.kids.values()][0];
        if (dota) for (const k of dota.kids.values()) k.open = true;
        // `doomed`: extras ticked, so the terracotta strike + the delete button can be seen
        if (mode === "doomed") for (const f of GV.files) if (f.owner === "extra") gv.sel.add(f.path);
        gvRebuild();
      } else if (mode === "modified") {
        // the DENSE page: 240 rows in one folder, which is where the folder/file distinction has
        // to survive a wall of near-identical paths
        gvGoPage("modified");
        for (const n of gv.root.kids.values()) n.open = true;
        const dota = [...gv.root.kids.values()][0];
        if (dota) for (const k of dota.kids.values()) k.open = true;
        gvRebuild();
      } else if (mode === "kept") {
        // only the pins: the review route for "what have I told the launcher to leave alone"
        gvGoPage("kept");
        for (const n of gv.root.kids.values()) n.open = true;
        gvRebuild();
      } else if (mode === "onlyextras") {
        // every row is an extra, so the extras page is the only one and it opens on it by itself
        for (const n of gv.root.kids.values()) n.open = true;
        gvRebuild();
      } else if (mode === "filter") {
        document.getElementById("gv-filter").value = "panorama";
        gv.filter = "panorama";
        gvRebuild();
      }
    } else if (h === "main:open") {
      // categories expanded: the member paths and their individual states, under the summary row —
      // including a manifest-tree heading ("Hero Demo Plus"), which must read as a plain category
      for (const id of ["__core", "gfx", "tree:/1"]) state.filesOpen.add(id);
      renderFiles(state.filesShown);
    } else if (h === "yours") {
      // the cheap "what is mine" view — pins plus what nothing claims, no verification behind it
      openFilesView(await invoke("your_files"), "settings", "yours");
      for (const n of gv.root.kids.values()) n.open = true;
      gvRebuild();
    } else if (h === "manage") {
      // the release has NOTHING pending, but files at our dests are the user's own: the primary
      // must read Manage, not Update, and the status must not say "up to date"
      applyCheck({ ...CHECK, changes: 0, userChanged: 2, primaryAction: "manage", canPlay: true,
        files: CHECK.files.filter((f) => f.status === "modified" || f.status === "kept") });
    } else if (h === "update") {
      // what pressing Update now opens when the release would replace files somebody edited
      openUpdateMenu(CHECK);
      for (const n of gv.root.kids.values()) n.open = true;
      gvRebuild();
    } else if (h === "nogame") {
      // the configured folder holds no game — only an interrupted download's cache
      applyCheck({ ...CHECK, gamePresent: false, pendingBaseBytes: 5.2 * GB,
        installed: false, changes: 0, files: [], options: [], canPlay: false, canUninstall: false });
    } else if (h === "gd:run") {
      // mid-download, ETA included: one synthetic 20-second-old rate sample, then a real tick
      // through onGdProgress so the line renders exactly as it would live
      document.getElementById("gd-title").textContent = t("gd.title");
      document.getElementById("gd-modal").classList.remove("hidden");
      gdStage("run");
      gd.perFile = new Map();
      // bytes are WIRE bytes since the bundle format (schema 3): 7.92 GB crosses the network
      // for 14.77 GB on disk, and a mid-run item is as often a packed bundle as a file
      gd.sum = 2.9 * GB; gd.doneFiles = 1289; gd.files = 4635; gd.bytes = 7.92 * GB;
      gd.samples = [{ t: performance.now() - 20000, b: 2.4 * GB }];
      gd.etaText = ""; gd.etaAt = 0;
      onGdProgress({ payload: { op: "game", item: "b002-txt-736453e4cf3c.phxb", current: 12,
        total: 146, bytesDone: 12 * 1024 * 1024, bytesTotal: 200 * 1024 * 1024, done: false } });
    } else if (h.startsWith("dest")) {
      // The destination stage. `dest` is the ordinary case (a subfolder inside an empty folder);
      // the rest are the states whose whole job is to say something, and they are rendered from a
      // crafted payload because the stub's canned one is deliberately the boring one.
      const mode = h.split(":")[1];
      const view = { prefix: "D:\\Games\\", path: "D:\\Games\\dota2_688f", nameError: null,
        defaultName: "dota2_688f", occupied: false, baseOccupied: false, foreignEntries: 0 };
      await gdDestOpen(null, "D:\\Games");
      if (mode === "flat") {
        // switched off: the game's own folder and our bookkeeping land beside the user's files
        gdDest.nest = false;
        gdDestRender({ ...view, prefix: "D:\\Games", path: "D:\\Games", foreignEntries: 12 });
      } else if (mode === "warn") {
        // pointed at a folder that already holds a game: nesting would be a second copy
        gdDestRender({ ...view, baseOccupied: true });
      } else if (mode === "bad") {
        $("gd-path-name").value = "dota:2";
        gdDest.name = "dota:2";
        gdDestRender({ ...view, path: null, nameError: "chars" });
      } else if (mode === "long") {
        // the width case: the picked path is longer than the card, so the head has to give way
        // while the segment being edited stays whole (and the "\" must not jump to the far left)
        const base = "D:\\SteamLibrary\\steamapps\\common\\dota 2 beta\\downloads";
        gdDestRender({ ...view, prefix: base + "\\", path: base + "\\dota2_688f" });
      }
    } else if (h === "gd") {
      document.getElementById("gd-title").textContent = t("gd.title");
      document.getElementById("gd-summary").textContent =
        t("gd.confirm", { gb: "7.9", disk: "14.8", n: 4635, dir: "D:\\Games\\Dota 2 6.88" });
      document.getElementById("gd-modal").classList.remove("hidden");
      gdStage("confirm");
    }

    // ?measure=1 — dump the horizontal geometry of the screen into the DOM, so
    // `chrome --headless=new --dump-dom` answers "where does this edge actually sit" instead of
    // a PNG being squinted at. Every number is a viewport x, so they compare directly.
    if (new URLSearchParams(location.search).has("measure")) {
      const box = document.createElement("pre");
      box.id = "measure";
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return sel + ": absent";
        const r = el.getBoundingClientRect();
        return sel + ": L=" + r.left.toFixed(1) + " R=" + r.right.toFixed(1) + " W=" + r.width.toFixed(1);
      };
      box.textContent = [
        "root-font-size: " + getComputedStyle(document.documentElement).fontSize,
        rect(".app"), rect("#gv-summary"), rect(".gv-legend"), rect("#gv-area"),
        rect("#gv-list"), rect(".gv-row"), rect(".gv-row .gv-twist"),
        rect(".gv-row .gv-count"), rect("#gv-total"),
      ].join("\n");
      document.body.append(box);
    }
  }, 900);
});
