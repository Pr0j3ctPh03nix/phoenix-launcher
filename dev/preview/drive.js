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
    setIdleStatus = () => {};
    // ?lang=ru — Russian labels are the long ones; worth a look before calling a layout done
    const lang = new URLSearchParams(location.search).get("lang");
    if (lang) await switchLang(lang);
    await doCheck();

    if (h.startsWith("settings")) {
      await openSettings();
      setSettingsTab(h.split(":")[1] || "general"); // settings:general | :launch | :files
    } else if (h === "setup") {
      showView("setup");
    } else if (h === "options") {
      renderOptions();
      showView("options");
    } else if (h === "confirm") {
      document.getElementById("btn-uninstall").click(); // the real handler, danger flag and all
    } else if (h === "verify" || h === "verify:stopping") {
      // mid-verify: the stub's game_verify never resolves, so the run state holds still. The
      // progress line is faked because the op-progress event has no stub to fire it.
      doGameVerify();
      setStatus(t("status.working"), "busy", t("gv.progress", { i: 1342, n: 4635 }));
      if (h.endsWith("stopping")) fireStop(); // the after-click state: Stop pressed, now disabled
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
      gd.sum = 2.9 * GB; gd.doneFiles = 1289; gd.files = 4635; gd.bytes = 14.75 * GB;
      gd.samples = [{ t: performance.now() - 20000, b: 2.4 * GB }];
      gd.etaText = ""; gd.etaAt = 0;
      onGdProgress({ payload: { op: "game", item: "game/dota/pak01_dir.vpk", current: 1290,
        total: 4635, bytesDone: 12 * 1024 * 1024, bytesTotal: 200 * 1024 * 1024, done: true } });
    } else if (h === "gd") {
      document.getElementById("gd-title").textContent = t("gd.title");
      document.getElementById("gd-summary").textContent =
        t("gd.confirm", { gb: "16.4", n: 4635, dir: "D:\\Games\\Dota 2 6.88" });
      document.getElementById("gd-modal").classList.remove("hidden");
      gdStage("confirm");
    }
  }, 900);
});
