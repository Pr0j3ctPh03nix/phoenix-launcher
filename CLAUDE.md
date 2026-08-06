# Project Phoenix Launcher

A Windows desktop app that keeps a **Dota 2 6.88 (build 1805)** install patched with the Project
Phoenix client shim. It downloads the latest release from a GitHub "dist" repo, verifies it, and
installs it into the game folder; it can also revert the game to stock and launch it.

Built with **Tauri 2** (Rust core + a WebView2 HTML/CSS/JS frontend). The Rust *engine* is
framework-agnostic; Tauri is only the shell.

## Where it fits

This repo (`Pr0j3ctPh03nix/phoenix-launcher`, public) is the **updater app** only. It does **not** contain
the shim. What it installs lives in a separate **dist repo** whose CI builds `winmm.dll` and publishes
a Release described by a `manifest.json`:

- `Pr0j3ctPh03nix/client-dist-staging` — private; the current default source (baked as
  `DEFAULT_REPO` in `config.rs`). Builds authenticate with a read-only token baked at build time
  (`PHOENIX_BAKED_TOKEN`, see `config.rs`); a user-saved token still wins.
- `Pr0j3ctPh03nix/client-dist` — public, for the eventual public release.

The updater is **data-driven**: the file list, install destinations, download URLs, and the
install-identity gate all come from the manifest. It hardcodes none of them. Change what ships, or
the target game build, by editing the dist repo + cutting a release — the updater needs no change.

## Layout

    src-tauri/            Rust
      src/
        main.rs           binary wiring only: module tree, Tauri builder, command registration
        cmd/              Tauri command layer, one module per domain (settings/update/notes/
                          launch/autofind/misc); AppState lives in cmd/mod.rs
        views.rs          the webview wire contract (view structs, camelCase) + CmdError
                          {kind,message} + build_check_view (UI-hint derivation)
        cli.rs            headless CLI (check/install/uninstall) for engine testing
        config.rs         Settings (schema-versioned, serialized Settings::update writes)
        downloader.rs     the network seam: Downloader trait + Release/Asset + NetKind error
                          marker + an in-memory fake for tests
        github.rs         GitHub Releases Downloader impl (public no-auth + private token)
        manifest.rs       manifest.json types (incl. min_launcher compat gate)
        steaminf.rs       reads game/dota/steam.inf ClientVersion (info only, no gating)
        verify.rs         sha256 of files / bytes ((size,mtime)-memoized)
        engine.rs         fetch (with the min_launcher gate) + resolve (options -> effective
                          file set) + plan (diff, incl. orphan Remove) + read-only `check` /
                          offline `evaluate` + OpProgress ticks; unit tests
        install.rs        install (game-running interlock, 2-phase, parallel resumable
                          downloads, rollback, orphan removal) + uninstall; unit tests
        state.rs          per-install record, stored in the game folder (corrupt -> quarantine)
        fslock.rs         shared Windows lock probes: probe() -> Writable / Held (sharing
                          violation = live process) / Denied (read-only, ACL) + held_by_process()
        launch.rs         spawns dota2.exe (base options + renderer + LAUNCH_FLAGS + extras)
                          + game_running
                          (write-probe of the exe image; a running process is locked)
        autofind.rs       game-folder scan: Steam libraries (registry/vdf) then all drives
      tauri.conf.json     window + bundle config
      capabilities/       Tauri 2 permissions
    frontend/             static HTML/CSS/JS (no bundler); fonts bundled here
      i18n.js             EN/RU string tables; static DOM via data-i18n, dynamic via t()
    dev/make_decoy.sh     builds a fake game folder for safe testing

## Build / run / test

WebView2 runtime required (present on current Win10/11).

    bun install                 # once — installs @tauri-apps/cli
    bun run tauri dev           # run the app
    bun run tauri build         # package (NSIS/MSI)

The frontend is static — editing `frontend/*` needs no recompile (reload the window). Editing
`tauri.conf.json` or `capabilities/` **does** recompile (baked at build via `generate_context!`).

**Headless engine test** (debug build keeps a console; reuses saved settings, flags override them):

    bash dev/make_decoy.sh                              # fake game folder (writes a steam.inf)
    cd src-tauri
    cargo run -- check     --game <dir> --repo <owner/name> [--token <t>]
    cargo run -- install   --game <dir> --repo <owner/name>
    cargo run -- uninstall --game <dir>

A token may also be passed via `PHOENIX_GITHUB_TOKEN` (keeps it out of argv). Always test install /
uninstall against a **decoy**, never a real game install.

## How it works

- **Downloader seam**: the engine never touches HTTP directly. `downloader.rs` holds the
  `Downloader` trait (+ `Release`/`Asset` types, the `NetKind` error marker, and an in-memory
  fake used by the install/engine tests); `github.rs` is the production impl. A new transport
  (mirror, resumable downloads) slots in without engine changes.
- **Error envelope**: every command fails with `CmdError {kind, message}` (views.rs). `kind` is
  classified from the anyhow chain (`network` / `auth` / `notFound` / `io` / `tooOld` /
  `gameRunning` / `internal`); the frontend reacts to it — the status word is kind-specific
  (Offline / No access / Launcher outdated / Game is running), the mono detail carries a
  localized hint plus the raw engine message.
- **Progress**: long engine ops emit `OpProgress` ticks through a `Progress` sink; the `apply`
  command forwards them to the webview as the `op-progress` event (`autofind` has its own
  `autofind-progress` event). CLI/tests pass `None`.
- **Compat gate**: a manifest may carry `min_launcher` (semver). Older launchers refuse it with
  a typed `TooOld` error (wire kind `tooOld`) instead of silently misinstalling a format they
  don't understand. Old manifests without the field are unaffected.
- **Game-running interlock**: install and uninstall first probe every file they're about to
  touch for write access (an open without truncating). A sharing violation means the game holds
  it open (loaded DLLs, mmapped VPKs) → typed `GameRunning` error, wire kind `gameRunning`,
  before a single byte is downloaded. Any OTHER can't-write (read-only attribute, ACL) is a
  distinct permission error (wire kind `io`) — "close Dota 2" must never be the diagnosis for a
  problem closing the game can't fix. std does NOT map sharing violations to PermissionDenied —
  `is_in_use` matches the raw Windows codes (5/32/33). Install re-probes after phase 1: the game
  may have started during a long download, and commit should fail typed and untouched, not
  mid-way into a best-effort rollback.
- **Downloads**: phase 1a fetches unique-by-hash files into the content-addressed cache with a
  4-worker pool (files are independent by construction); staging copies stay sequential so
  commit is unchanged. Progress ticks fan out to every dest sharing an asset hash (each UI row
  gets its bar). The first failure aborts the sibling in-flight streams at their next chunk
  (`ChunkProgress` returns bool; false = abort, partial kept) — a dead asset never waits minutes
  for a huge neighbor to finish before surfacing. An interrupted download keeps its `.part`; the
  next run resumes it via a Range request (the returned sha covers the whole file — the prefix
  is re-hashed). A hash mismatch deletes the `.part` (corrupt bytes can't resume).
  File-granularity resume comes free from the cache itself.
- **Cache warm**: after a successful apply the shell runs `install::warm_cache` DETACHED — it
  refetches the release (one API call) and prefetches every remaining manifest asset
  (unselected variants, disabled toggles) so customization flips never wait on the network.
  Optional content can be hundreds of MB, so it must never block the install result / Play
  unlock. Best-effort throughout; uninstall stops it via `install::cancel_warm` (shell calls
  it — the engine `uninstall` stays free of process-global state), which also aborts a
  download mid-file via the chunk callback (a leftover `.part` at worst). A process-wide
  in-flight hash guard stops a warm and an apply from writing the same `.part` concurrently.
- **Settings** persist to the OS config dir (`directories` crate); the GitHub token is never sent
  back to the UI (only a `hasToken` flag, and it reflects a user-saved token only). A blank token
  field keeps the saved one; removing it takes the explicit `clear_token` flag (the UI's Clear
  button). The build-time baked token is merged at the point of use (`Settings::token()`), never
  into the persisted struct — a settings save can't write it to disk. Also persisted: `language`
  (en/ru), `launch_extra`, `renderer` (dx11/dx9), `launch_flags` (see Launch), and `selections`
  (manifest option id -> variant
  id or bool). Settings and the install state file are written temp+rename (atomic on the same
  volume) — a crash mid-write can't torch them; the quarantine/.bak loaders stay last resorts.
  Saving a change to the game folder, repo, or token invalidates the check view and re-checks —
  Play must never act on a folder the visible status doesn't describe.
- **Manifest options (v2)**: top-level `options[]` — `kind:"choice"` (N `variants` sharing one
  `dest`, `default` = variant id) and `kind:"toggle"` (`files[]` installed when on, `default` =
  bool). Labels are plain strings or `{"en":…,"ru":…}` maps. `engine::resolve()` materializes the
  effective file set from selections; deselected files the previous install placed surface as
  `Action::Remove` (orphans) and are deleted on the next apply (vanilla originals restored). Old
  manifests without `options` parse unchanged. The dist repo's `gen_manifest.py` must emit this —
  spec lives in this repo's plan/history, updater side is done.
- **Manifest cache**: the last fetched manifest is kept in memory; the `replan` command re-diffs
  selections against it with no network (drives the Customization view). A successful `apply`
  refreshes the cache from the release it actually installed (`InstallReport` carries the
  manifest + tag), so the frontend's post-apply and post-uninstall refreshes are offline-safe
  replans — a successful offline uninstall can never end in a red network error.
- **Launch**: `play` runs `game/bin/win64/dota2.exe` with hardcoded
  `-insecure -console +exec autoexec.cfg` + `-dx11`/`-dx9` + enabled `LAUNCH_FLAGS` + user extras
  (user's last, so a duplicated option lands on their value). `launch::LAUNCH_FLAGS` is the single
  source of truth for the optional flags settings expose as switches (id + args + default; today
  `noCloudKeybinds` -> `+dota_keybindings_cloud_disable 1`): the settings view, `save_settings`
  and the spawn all read it, so a new flag = one row + a `set.flag.<id>` string in i18n.js (with
  no string the UI shows the raw args). `settings.launch_flags` (id -> bool) stores only ids the
  table knows — a missing id means the flag's own default, so new flags need no migration and a
  stale key from another build can't inject arguments. Play is enabled in the
  UI only when installed with no pending changes; Check is always available. The autoexec.cfg
  editor reads bytes: a non-UTF-8 file (cp1251 comments are real) comes back lossy-decoded with
  a `lossy` flag and the editor goes read-only — saving a lossy decode (or after a failed read)
  would corrupt/blank the user's real cfg.
- **Game tracking**: the frontend polls the `game_running` command every 3 s (a write-probe of
  the dota2.exe image via `fslock::held_by_process` — ONLY sharing/lock violations count, so a
  read-only or ACL-denied exe is never mistaken for a running game). While the game runs the
  status shows "In game" and Play/Uninstall are disabled (the backend interlock is the second
  line); when in "check" mode the primary stays a live Check button (read-only is always
  allowed). When the game closes, one offline `replan` refreshes the status — no network, so
  closing the game while offline can't flip "Up to date" into an error (full `check` only when
  nothing is cached yet).
- **Autofind**: fast pass over Steam libraries (HKCU SteamPath + libraryfolders.vdf), then a
  bounded walk (depth 6, pruned system dirs) of all drives for `game/dota/steam.inf`; progress via
  the `autofind-progress` Tauri event, cancel via AtomicBool. Candidates carry their found
  ClientVersion purely as display info.
- **First run**: setup view (Browse / Autofind) shows only when no game folder was ever chosen AND
  the exe's own dir has no `game/dota/steam.inf`; any picked folder is accepted without validation.
- **Window state**: position + maximized persist across runs via `tauri-plugin-window-state`
  (flags POSITION | MAXIMIZED only — size always resets to the config default, and visibility
  is NOT persisted: hidden-until-first-paint stays frontend-managed). First run centers the
  window (`center: true`); after that the saved spot wins, validated against connected monitors.
  Closing the window while an op runs asks first (`onCloseRequested` + confirm) — downloads
  resume, but a phase-2 commit must not be killed cold. GOTCHA: once JS registers an
  `onCloseRequested` listener, Tauri no longer closes the window itself — the JS wrapper calls
  `destroy()`, which needs `core:window:allow-destroy` in `capabilities/default.json` (NOT part
  of `core:default`; without it the X silently does nothing).
- **What's new**: the history rebuild downloads unseen releases' manifests with a small worker
  pool (`NOTES_WORKERS`) — a first-ever open is not N serial round trips. Known tags still cost
  nothing; cache stays memory + disk, keyed by the last checked tag.
- **i18n**: all labels derive in the frontend (it owns the language); Rust ships raw data + hints
  (`primary_action`, `can_play`, …). Engine error strings stay English (shown in the mono detail).
- **check**: GitHub API → release (by tag or latest) → download `manifest.json` → sha256 each
  managed file vs the manifest → return a view (status, per-file action, next primary action).
  **No build gating**: install goes into whatever folder the user chose; `steam.inf` is read only
  as info (autofind candidate display, first-run heuristic). The manifest's `requires_install` is
  parsed for compatibility but ignored.
- **install** is two-phase so a real game is never left half-updated:
  1. download every changed file, verify sha256 **and** size, into `<game>/.phoenix-staging/`;
  2. commit: back up each existing target, atomically move the staged file in, create
     `winmm_orig.dll`, apply the manifest `remove[]`, write state. Any phase-2 failure rolls back.
  A **no-op install still heals**: when every resolved file already hash-matches, install writes
  the state file and creates a missing `winmm_orig.dll` anyway — a lost/corrupt
  `.phoenix-state.json` can never wedge the folder into "up to date but not installed" (the UI
  offers Apply for exactly this case: `primary_action` is `apply` when `changes == 0 &&
  !installed`, labeled **Repair** frontend-side). Removals: a file at a `remove[]`/orphan dest
  that we did NOT place is moved into `.phoenix-vanilla/` (preserved, not destroyed) and the
  removal sticks — the restore-a-vanilla-original step only fires for a copy that predates the
  removal, never for the one `back_up` just created (that would silently undo the removal and
  re-flag it on every plan, forever).
- **uninstall** reverts to stock from `<game>/.phoenix-state.json`: restore a preserved vanilla
  original if one exists, else delete our file; then restore anything still left in the vanilla
  store (files preserved by removals — not in `state.files`) to its game path; delete
  `winmm_orig.dll` only if *we* created it. A corrupt state file is quarantined to
  `.phoenix-state.json.bak` on load (treated as not-installed) rather than silently misread.

## Invariants & gotchas — do not break these

- **No install gate** (removed by decision 2026-07-30): the updater installs into any chosen
  folder; wrong-build installs are the user's responsibility. (Historical note: the real 6.88
  build is ClientVersion `1805`; `6869` was a polluted value — never use it as a reference.)
- **`winmm_orig.dll`** is a copy of the system `winmm.dll`. **Never overwrite an existing one** —
  that would make the proxy's forwarders point at themselves. Uninstall deletes it only when
  `state.winmm_orig_created` is true.
- **Clean uninstall** relies on every shipped file being a net-new loose override (its stock form is
  in the VPK / System32). If a file ever shadows a genuine vanilla *loose* file, `install.rs` already
  preserves that original under `.phoenix-vanilla/` for restore — keep that path intact.
- **Staging must be on the same volume as the game** (`<game>/.phoenix-staging/`) so the final move
  is atomic. Do not move it to `%TEMP%`.
- **Tauri command args**: camelCase in JS → snake_case in Rust (Tauri converts). `withGlobalTauri` is
  on, so the frontend calls `window.__TAURI__.core.invoke(...)` — no imports, no bundler.
- **No white flash**: the window is created hidden (`visible:false`) and revealed from JS after the
  first paint. Keep that.
- **GitHub downloads**: public via `browser_download_url`; private via the API asset URL + token,
  following the 302 to storage **without** forwarding the auth header (pre-signed URLs reject it).

## Design (frontend)

Minimal by decision: near-black ground (`#0f0d12`), one quiet centered column, hairline structure.
Meaning lives in color only: **gold = action/attention**, **verdigris (`#62d0b4`) =
settled/up-to-date**, **terracotta = error**. Fonts (bundled, `@font-face`): Inter (UI) and
JetBrains Mono (paths/data). Sizing is fluid — everything is rem and the root font-size clamps on
vmin, so the whole UI scales with the window. Structural rhythm tokens (`--gap-head`, `--band`,
`--gutter`, `--marker`, `--ctl-h`, `--btn-min`) live in `:root` — retune the whole column from
there. Ring-spinner loader, staggered `.rise` reveal on main, `prefers-reduced-motion` honored.

## Editing conventions

- The **engine** (config/downloader/github/manifest/install/state/steaminf/verify/engine) stays
  UI-agnostic and pure Rust — no Tauri types in it, network only through the `Downloader` trait.
  Tauri commands live in `cmd/`, the wire contract (view structs, `CmdError`, UI-hint derivation)
  in `views.rs`.
- Settings writes go through `Settings::update` (load → mutate → save, serialized), never a
  hand-rolled load/save pair in a command.
- Prefer adding to the manifest over hardcoding in the updater. The updater should stay a dumb,
  data-driven installer.
