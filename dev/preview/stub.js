// Preview-only stub of the Tauri bridge: canned command results so the real frontend renders in a
// plain browser. Never shipped — lives in the scratchpad copy only.
const CHECK = {
  status: "update",
  version: "v1.2.1",
  gameDir: "D:\\Games\\Dota 2 6.88",
  installed: true,
  changes: 7,
  userChanged: 3,
  canPlay: true,
  canUninstall: true,
  gamePresent: true,
  pendingBaseBytes: 0,
  primaryAction: "apply",
  notes: "## Fixed\n- Launch tweaks\n- Release build caching",
  options: [{ id: "hud", kind: "choice", label: "HUD skin", value: "classic", variants: [{ id: "classic", label: "Classic" }, { id: "dark", label: "Dark" }] }],
  files: [
    { dest: "game\\bin\\win64\\winmm.dll", status: "update" },
    { dest: "game\\dota\\pak01_dir.vpk", status: "ok" },
    { dest: "game\\dota\\cfg\\autoexec.cfg", status: "install" },
    // a manifest-tree heading: collapses like an option's set but stays a plain category —
    // core glyph, no checkbox semantics (treeGroup is what renderFiles keys that on)
    { dest: "game\\dota_addons_phoenix\\hero_demo\\scripts\\vscripts\\events.lua", status: "install",
      groupId: "tree:/1", treeGroup: true, group: { en: "Hero Demo Plus", ru: "Hero Demo Plus" } },
    { dest: "game\\dota_addons_phoenix\\polygon\\scripts\\vscripts\\lasthit.lua", status: "install",
      groupId: "tree:/1", treeGroup: true, group: { en: "Hero Demo Plus", ru: "Hero Demo Plus" } },
    // an option-owned set: renders as ONE "New graphics" row, not three paths
    { dest: "game\\dota_phoenix\\textures_a.vpk", status: "update", groupId: "gfx", group: { en: "New graphics", ru: "Новая графика" } },
    { dest: "game\\dota_phoenix\\textures_b.vpk", status: "update", groupId: "gfx", group: { en: "New graphics", ru: "Новая графика" } },
    { dest: "game\\dota_phoenix\\particles.vpk", status: "ok", groupId: "gfx", group: { en: "New graphics", ru: "Новая графика" } },
    // a choice's shared dest: shows "HUD skin · Classic", never the path
    { dest: "game\\dota_phoenix\\hud.vpk", status: "ok", groupId: "hud", group: "HUD skin", variant: "Classic" },
    { dest: "game\\dota\\stale_override.vpk", status: "remove" },
    // files somebody edited: the update menu is the screen that asks what to do about them
    { dest: "game/dota/cfg/autoexec.cfg", status: "modified", localSize: 730, mtime: 1754990000 },
    // your bytes AND a newer release version of the same file: "modified / update"
    { dest: "game/dota_phoenix/hud_custom.vpk", status: "modified", updateAvailable: true, localSize: 2900000, mtime: 1754960000 },
    { dest: "game/dota_phoenix/particles.vpk", status: "kept", localSize: 5400000, mtime: 1753200000 },
    // the case that used to vanish: kept, and this release changes it
    // kept, and the release has since moved on: reads "kept / update", unticked by default
    { dest: "game/dota_phoenix/lighting.vpk", status: "kept", updateAvailable: true, localSize: 3300000, mtime: 1752000000 },
    // pins on dests the shim does NOT manage — a vanilla file somebody modded. These have no
    // group and are not shim files, so they land in the "Your files" category.
    { dest: "game/dota/resource/flash3/dota_english.txt", status: "kept", yours: true, localSize: 902400, mtime: 1753000000 },
    { dest: "game/dota/panorama/images/custom/logo.png", status: "kept", yours: true, localSize: 240000, mtime: 1754960000 },
  ],
};

// ?lang=ru must reach boot()'s own setLang, not just the director: boot runs LATE here (its rAF
// is only serviced when the capture forces a frame) and would re-apply the settings language
// over everything static.
const LANG_Q = new URLSearchParams(location.search).get("lang") || "en";

// ?fail=check makes the network commands reject like an offline machine would, so the local-only
// fallback (and any other can't-reach-GitHub wording) can be seen.
const FAIL = new URLSearchParams(location.search).get("fail");
const offline = { kind: "network", message: "fetching the release: connection failed" };

const HANDLERS = {
  get_settings: () => ({
    sourceRepo: "Pr0j3ctPh03nix/client-dist-staging",
    gameDir: "D:\\Games\\Dota 2 6.88",
    hasToken: false,
    language: LANG_Q,
    launchExtra: "-novid",
    renderer: "dx11",
    animations: true,
    launchFlags: [],
  }),
  launcher_info: () => ({ version: "1.2.1", justUpdated: false }),
  game_dir_status: () => ({ configured: true, clientVersion: "1805" }),
  game_running: () => false,
  check: () => {
    if (FAIL === "check") throw offline;
    return CHECK;
  },
  // the install-record-only verdict the frontend falls back to when `check` fails
  local_check: () => ({
    ...CHECK,
    tag: "",
    changes: 0,
    notes: null,
    options: [],
    files: CHECK.files.filter((f) => f.status !== "remove").map((f) => ({ ...f, status: "ok" })),
    primaryAction: "check",
    canPlay: true,
    canUninstall: true,
    local: true,
  }),
  replan: () => CHECK,
  launcher_check: () => {
    if (FAIL === "check") throw offline;
    return null;
  },
  // ?fail=check reaches these too: the What's-new panes render whatever they already know and put
  // the reason on their own message line, which is a layout worth being able to look at
  release_notes: () => {
    if (FAIL === "check") throw offline;
    return [
    {
      tag: "v1.2.1", version: "v1.2.1",
      notes: "#### Added\n- New HUD skin option\n\n#### Changed\n- Faster release build caching\n\n#### Fixed\n- Launch tweaks\n- Crash on exit with `-novid`",
    },
    { tag: "v1.2.0", version: "v1.2.0", notes: "#### Fixed\n- Launch tweaks" },
    ];
  },
  // the launcher's own history — a DIFFERENT repo on a different version line, which is the whole
  // reason it is a second page. 1.2.1 is what launcher_info reports, so it wears the "current" pill.
  launcher_notes: () => {
    if (FAIL === "check") throw offline;
    return [
    { tag: "v1.3.0", version: "1.3.0", notes: "#### Added\n- Two-page What's new: the mod's releases and the launcher's own\n\n#### Fixed\n- Dialog copy no longer runs two sentences into one line" },
    { tag: "v1.2.1", version: "1.2.1", notes: "#### Changed\n- Faster startup check\n\n#### Fixed\n- Leftover `.old.exe` after a self-update" },
    { tag: "v1.2.0", version: "1.2.0", notes: "#### Added\n- Self-update" },
    ];
  },
  read_autoexec: () => ({
    content: '// comment\ndota_camera_distance "1200"\ncl_updaterate 128\ncl_cmdrate 128; echo hi\n',
    lossy: false,
    pinned: ["cl_updaterate", "cl_cmdrate"],
  }),
  set_language: () => null,
  save_settings: () => null,
  set_selection: () => null,
  open_url: () => null,
  browse_folder: () => "D:\\Games\\Dota 2 6.88",
  // never resolves: the #verify screen is the MID-RUN state (Working… + Stop), which only exists
  // while the backend is still hashing. main.js destructures `invoke` at load, so the freeze has
  // to live here — drive.js runs too late to wrap it.
  game_verify: () => new Promise(() => {}),
  // the cheap read: pins + whatever nothing claims, no integrity pass. A subset of GV by
  // construction, so it is built from the same rows.
  your_files: () => ({
    ...GV,
    total: 6, ok: 0, skipped: 0,
    files: GV.files.filter((f) => f.state === "kept" || f.owner === "extra" ||
      (f.owner === "phoenix" && f.state === "modified")),
    kept: GV.files.filter((f) => f.state === "kept").length,
    damagedBytes: 0,
  }),
  // The destination resolver behind the download dialog's first stage. The COMPOSITION is the real
  // rule (a base that already ends in a separator must not be given a second one) and so is the
  // name check, because both are what that screen is for; the "what is already there" fields are
  // canned — drive.js crafts those cases directly.
  game_target: ({ base, sub }) => {
    const prefix = sub == null || /[\\/]$/.test(base) ? base : base + "\\";
    const err =
      sub == null ? null
      : sub === "" ? "empty"
      : /[\\/]/.test(sub) ? "sep"
      : /[:*?"<>|]/.test(sub) ? "chars"
      : sub !== sub.trim() || sub.endsWith(".") ? "edge"
      : null;
    return {
      prefix,
      path: err ? null : prefix + (sub ?? ""),
      nameError: err,
      defaultName: "dota2_688f",
      occupied: false,
      baseOccupied: false,
      foreignEntries: 0,
    };
  },
  game_cancel: () => null,
  game_repair: () => ({ gameVersion: "6.88f", written: 0, upToDate: 0, bytes: 0 }),
  game_delete_extras: (a) => a.paths.length,
  phoenix_keep: (a) => a.keep.length,
};

// A files-view payload with every state in it, at a scale that actually exercises the tree: a
// couple of real problems, a mod that replaced files and added its own, a pinned file, and one
// summarized addon folder. `#files` in drive.js renders this.
const GV = (() => {
  const files = [];
  const add = (path, owner, state, o = {}) => files.push({
    path, owner, state,
    size: o.size ?? 4096, localSize: o.localSize ?? o.size ?? 4096,
    mtime: o.mtime ?? 1754700000, wireKey: o.wireKey ?? "bundle:b002", wire: o.wire ?? 88_000_000,
    files: o.files ?? 0,
  });
  // genuine damage: one truncated, one gone, one locked
  add("game/dota/pak01_dir.vpk", "game", "modified", { size: 356_000_000, localSize: 2_100_000, mtime: 1754900000 });
  add("game/core/pak01_dir.vpk", "game", "missing", { size: 121_000_000, localSize: null, mtime: null });
  add("game/dota/maps/dota.vpk", "game", "unreadable", { size: 88_000_000, localSize: 88_000_000 });
  // a HUD mod: replaced panorama files, plus files of its own
  for (let i = 0; i < 240; i++) {
    add(`game/dota/panorama/layout/custom_game/hud_${i}.vxml_c`, "game", "modified",
        { size: 12_400, localSize: 15_900, mtime: 1754960000 });
  }
  add("game/dota/panorama/images/custom/logo.png", "extra", "extra", { localSize: 240_000, mtime: 1754960000 });
  // a big map pack the manifest knows nothing about
  add("game/dota/addons", "extra", "extraDir", { localSize: 1_400_000_000, files: 4128, mtime: 1754300000 });
  // already approved once — one per authority, so the summary line's "kept" total and the chip's
  // count are exercised against BOTH (they disagreed when the total came off the base plan alone)
  add("game/dota/resource/flash3/dota_english.txt", "game", "kept", { size: 900_000, localSize: 902_400, mtime: 1753000000 });
  add("game/dota_phoenix/particles.vpk", "phoenix", "kept", { size: 5_100_000, localSize: 5_400_000, mtime: 1753200000, wireKey: "phx3", wire: 5_100_000 });
  // Phoenix files the user edited — one among the game's own, one in Phoenix's own tree. The
  // second is the shape that used to be misreported as a foreign folder.
  add("game/dota/cfg/autoexec.cfg", "phoenix", "modified", { size: 512, localSize: 730, mtime: 1754990000, wireKey: "phx1", wire: 512 });
  add("game/dota_phoenix/hud.vpk", "phoenix", "modified", { size: 2_400_000, localSize: 2_900_000, mtime: 1754990000, wireKey: "phx2", wire: 2_400_000 });
  return {
    version: "6.88f", total: 4635, ok: 4635 - files.length, skipped: 0,
    kept: files.filter((f) => f.state === "kept").length,
    files, damagedBytes: 1_180_000_000, extrasTruncated: false,
    foreignBuild: false, phoenixUnknown: false,
  };
})();

window.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      const h = HANDLERS[cmd];
      if (!h) throw { kind: "internal", message: "stub: no handler for " + cmd };
      return h(args);
    },
  },
  event: { listen: async () => () => {} },
  window: { getCurrentWindow: () => ({ show: async () => {}, destroy: () => {}, onCloseRequested: () => {} }) },
};
