# Project Phoenix Launcher

A Windows desktop app that keeps a **Dota 2 6.88 (build 1805)** install patched with the Project
Phoenix client shim. It downloads the latest release from a GitHub "dist" repo, verifies it, and
installs it into the game folder; it can also revert the game to stock and launch it.

Built with **Tauri 2** (Rust core + a WebView2 HTML/CSS/JS frontend). The Rust *engine* is
framework-agnostic; Tauri is only the shell.

## Where it fits

This repo (`Pr0j3ctPh03nix/auto-updater`, public) is the **updater app** only. It does **not** contain
the shim. What it installs lives in a separate **dist repo** whose CI builds `winmm.dll` and publishes
a Release described by a `manifest.json`:

- `Pr0j3ctPh03nix/client-dist` — public, the default source (baked as `DEFAULT_REPO` in `config.rs`).
- `Pr0j3ctPh03nix/client-dist-staging` — private, for testing releases (needs a token).

The updater is **data-driven**: the file list, install destinations, download URLs, and the
install-identity gate all come from the manifest. It hardcodes none of them. Change what ships, or
the target game build, by editing the dist repo + cutting a release — the updater needs no change.

## Layout

    src-tauri/            Rust
      src/
        main.rs           Tauri command layer (wraps the engine) + a headless CLI for testing
        config.rs         Settings: source_repo / game_dir / token, persisted via `directories`
        github.rs         GitHub Releases client (public no-auth + private token, same code path)
        manifest.rs       manifest.json types
        steaminf.rs       reads game/dota/steam.inf ClientVersion (info only, no gating)
        verify.rs         sha256 of files / bytes
        engine.rs         fetch + resolve (options -> effective file set) + plan (diff, incl.
                          orphan Remove) + read-only `check` / offline `evaluate`; unit tests
        install.rs        install (2-phase, rollback, orphan removal) + uninstall (revert to stock)
        state.rs          per-install record, stored in the game folder
        launch.rs         spawns dota2.exe with base options + renderer flag + user extras
        autofind.rs       game-folder scan: Steam libraries (registry/vdf) then all drives
      tauri.conf.json     window + bundle config
      capabilities/       Tauri 2 permissions
    frontend/             static HTML/CSS/JS (no bundler); fonts + phoenix image bundled here
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

- **Settings** persist to the OS config dir (`directories` crate); the GitHub token is never sent
  back to the UI (only a `hasToken` flag). Also persisted: `language` (en/ru), `launch_extra`,
  `renderer` (dx11/dx9), and `selections` (manifest option id -> variant id or bool).
- **Manifest options (v2)**: top-level `options[]` — `kind:"choice"` (N `variants` sharing one
  `dest`, `default` = variant id) and `kind:"toggle"` (`files[]` installed when on, `default` =
  bool). Labels are plain strings or `{"en":…,"ru":…}` maps. `engine::resolve()` materializes the
  effective file set from selections; deselected files the previous install placed surface as
  `Action::Remove` (orphans) and are deleted on the next apply (vanilla originals restored). Old
  manifests without `options` parse unchanged. The dist repo's `gen_manifest.py` must emit this —
  spec lives in this repo's plan/history, updater side is done.
- **Manifest cache**: the last fetched manifest is kept in memory; the `replan` command re-diffs
  selections against it with no network (drives the Customization view).
- **Launch**: `play` runs `game/bin/win64/dota2.exe` with hardcoded
  `-insecure -console +exec autoexec.cfg` + `-dx11`/`-dx9` + user extras. Play is enabled in the
  UI only when installed with no pending changes; Check is always available.
- **Autofind**: fast pass over Steam libraries (HKCU SteamPath + libraryfolders.vdf), then a
  bounded walk (depth 6, pruned system dirs) of all drives for `game/dota/steam.inf`; progress via
  the `autofind-progress` Tauri event, cancel via AtomicBool. Candidates carry their found
  ClientVersion purely as display info.
- **First run**: setup view (Browse / Autofind) shows only when no game folder was ever chosen AND
  the exe's own dir has no `game/dota/steam.inf`; any picked folder is accepted without validation.
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
- **uninstall** reverts to stock from `<game>/.phoenix-state.json`: restore a preserved vanilla
  original if one exists, else delete our file; delete `winmm_orig.dll` only if *we* created it.

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

"The Keeper's Console" — dark warm charcoal, **antique gold** as the one accent, over a blurred
phoenix aura behind the wordmark. Meaningful color: **gold = action/attention**, **verdigris
(`#62d0b4`) = settled/up-to-date**, **terracotta = error**. Fonts (bundled, `@font-face`): Marcellus
(wordmark), Inter (UI), JetBrains Mono (paths/data). Engraved cards, a struck-gold-seal primary
button, a hexagon-trace startup loader. Keep it minimal — one bold element (the phoenix), everything
else quiet. Tunable knobs are commented in `style.css` (phoenix blur/opacity, mask).

## Editing conventions

- The **engine** (config/github/manifest/install/state/steaminf/verify/engine) stays UI-agnostic and
  pure Rust — no Tauri types in it. Tauri view structs + the derivation of UI hints live in `main.rs`.
- Prefer adding to the manifest over hardcoding in the updater. The updater should stay a dumb,
  data-driven installer.
