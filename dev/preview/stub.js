// Preview-only stub of the Tauri bridge: canned command results so the real frontend renders in a
// plain browser. Never shipped — lives in the scratchpad copy only.
const CHECK = {
  status: "update",
  version: "v1.2.1",
  gameDir: "D:\\Games\\Dota 2 6.88",
  installed: true,
  changes: 2,
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
    { dest: "game\\dota\\stale_override.vpk", status: "remove" },
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
    launchFlags: [{ id: "noCloudKeybinds", args: "+dota_keybindings_cloud_disable 1", enabled: true }],
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
  release_notes: () => [{ version: "v1.2.1", notes: "## Fixed\n- Launch tweaks" }],
  read_autoexec: () => ({ content: '// comment\ndota_camera_distance "1200"\n', lossy: false }),
  set_language: () => null,
  save_settings: () => null,
  set_selection: () => null,
  open_url: () => null,
  browse_folder: () => "D:\\Games\\Dota 2 6.88",
  // never resolves: the #verify screen is the MID-RUN state (Working… + Stop), which only exists
  // while the backend is still hashing. main.js destructures `invoke` at load, so the freeze has
  // to live here — drive.js runs too late to wrap it.
  game_verify: () => new Promise(() => {}),
  game_cancel: () => null,
};

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
