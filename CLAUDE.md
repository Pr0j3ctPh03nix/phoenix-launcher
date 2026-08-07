# Project Phoenix Launcher

A Windows desktop app that keeps a **Dota 2 6.88 (build 1805)** install patched with the Project
Phoenix client shim. It downloads the latest release from a GitHub "dist" repo, verifies it, and
installs it into the game folder; it can also revert the game to stock and launch it.

Built with **Tauri 2** (Rust core + a WebView2 HTML/CSS/JS frontend). The Rust *engine* is
framework-agnostic; Tauri is only the shell.

## Where it fits

This repo (`Pr0j3ctPh03nix/phoenix-launcher`) is the **updater app** only. It does **not** contain
the shim. It is also what the launcher **self-updates** from (see Self-update): its Releases
publish the portable `phoenix-launcher.exe` plus a `.sha256` sidecar. NOTE: intended to be public,
but currently **private** — verified 2026-08-06, the API 404s anonymously. Self-update therefore
falls back to the dist token; making the repo public is what lets it work token-free. What it installs lives in a separate **dist repo** whose CI builds `winmm.dll` and publishes
a Release described by a `manifest.json`:

- `Pr0j3ctPh03nix/client-dist-staging` — private; the current default source (baked as
  `DEFAULT_REPO` in `config.rs`). Builds authenticate with a read-only token baked at build time
  (`PHOENIX_BAKED_TOKEN`, see `config.rs`); a user-saved token still wins.
- `Pr0j3ctPh03nix/client-dist` — public, for the eventual public release.
- `Pr0j3ctPh03nix/game-dist` — public (must be: game downloads are GBs and ride the tokenless
  `browser_download_url` CDN path), holds the vanilla Dota 2 build-1805 files as release assets
  + a `manifest.json` in the standard format. The base for fresh installs / Verify game files /
  repair. **Sharded**: GitHub caps a release at 1,000 assets and the tree is 4,635 files, so the
  versioned release (v1805, always `/releases/latest` — shards are prereleases, which latest
  never resolves to) carries manifest.json while `v1805-assets-N` prereleases carry the files,
  **≤190 each** (~25 shards): the launcher reads assets from the list-releases response's
  EMBEDDED arrays, which are only verified complete up to ~200 assets (probed against a real
  195-asset release; GitHub documents no bound) — capping at 190 keeps every shard inside proven
  territory with zero pagination. `engine::merged_game_release` folds every shard back into one
  asset index, so the download machinery keeps its single-release worldview. Produced by the
  dist repo's `tools/gen_game_manifest.py` from a pristine install. Kept separate on purpose:
  a takedown of game bytes must not sink the launcher/shim infrastructure. **It must stay
  public**: `open_repo` succeeds anonymously, so assets download via `browser_download_url` —
  no auth, no API budget. Private, every one of the 4,635 assets would instead be an API
  request against the token owner's 5,000/hr limit, i.e. roughly one fresh install per hour
  shared across all users. The baked token therefore needs NO access to this repo.

The updater is **data-driven**: the file list, install destinations, download URLs, and the
install-identity gate all come from the manifest. It hardcodes none of them. Change what ships, or
the target game build, by editing the dist repo + cutting a release — the updater needs no change.

## Layout

    src-tauri/            Rust
      src/
        main.rs           binary wiring only: module tree, Tauri builder, command registration
        cmd/              Tauri command layer, one module per domain (settings/update/notes/
                          launch/autofind/misc/selfupdate/game); AppState + the shared
                          `open_repo` (anon-first repo auth) + `begin_op` live in cmd/mod.rs
        views.rs          the webview wire contract (view structs, camelCase) + CmdError
                          {kind,message} + build_check_view (UI-hint derivation)
        cli.rs            headless CLI (check/install/uninstall) for engine testing
        config.rs         Settings (schema-versioned, serialized Settings::update writes)
        downloader.rs     the network seam: Downloader trait + Release/Asset + NetKind error
                          marker + an in-memory fake for tests
        github.rs         GitHub Releases Downloader impl (public no-auth + private token)
        manifest.rs       manifest.json types + `Manifest::parse` (the `schema` compat gate and
                          dest-traversal rejection); conformance tests walk manifest-fixtures/
        steaminf.rs       reads game/dota/steam.inf ClientVersion (info only, no gating)
        verify.rs         sha256 of files ((size,mtime)-memoized)
        engine.rs         fetch (via Manifest::parse) + resolve (options -> effective
                          file set) + plan (diff, incl. orphan Remove) + read-only `check` /
                          offline `evaluate` + OpProgress ticks; unit tests
        install.rs        install (game-running interlock, 2-phase, parallel resumable
                          downloads, rollback, orphan removal) + uninstall + the base-game
                          pipeline (base_plan/install_base: fresh install, verify, repair);
                          unit tests
        selfupdate.rs     launcher self-update: is a release newer than this build + verified
                          exe swap (rename-the-running-exe) + `.old` leftover cleanup
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
    dev/shoot.sh          headless screenshots of the frontend, no build
      preview/            its stub `window.__TAURI__` + location.hash director (never shipped)
    dev/check_i18n.js     EN/RU key parity + every `$("id")` resolves; exits non-zero on a gap
    known_bugs.md         accepted residual risks: trigger, why accepted, fix direction

## Build / run / test

WebView2 runtime required (present on current Win10/11).

    bun install                 # once — installs @tauri-apps/cli
    bun run tauri dev           # run the app
    bun run tauri build         # build the portable exe (bundle.active is false — no NSIS/MSI)

    # boot into the FIRST-RUN setup view (debug-only). The env prefix is bash/git-bash syntax;
    # PowerShell:  $env:PHOENIX_FORCE_SETUP = "1"; bun run tauri dev
    #              (Remove-Item Env:\PHOENIX_FORCE_SETUP afterwards — it persists per terminal)
    PHOENIX_FORCE_SETUP=1 bun run tauri dev

App/exe icon: `src-tauri/icons/` is generated with `bun run tauri icon <png>` from the master at
`E:\project-phoenix\src\phx_icon.png` (outside this repo) — the master has an OPAQUE black
background, so regeneration must go through the background-key + square-pad step first
(flood-fill from the borders, so the artwork's own dark interior survives; a naive black key
would punch holes in it).

`PHOENIX_FORCE_SETUP` makes `game_dir_status` report "never configured, no game beside the exe"
(the condition boot() shows setup on) without touching saved settings — the only way to see that
view once a folder was ever chosen. Read-side only: actually picking a folder in it saves for
real. For a TRUE first run (no settings, language auto-detect), move
`%APPDATA%\ProjectPhoenix\PhoenixLauncher\config\settings.json` aside (notes_cache.json sits next
to it). Layout-only screenshots of the same view: `bash dev/shoot.sh setup`.

The frontend is static — editing `frontend/*` needs no recompile (reload the window). Editing
`tauri.conf.json` or `capabilities/` **does** recompile (baked at build via `generate_context!`).

**Frontend screenshots without building** — the way to actually LOOK at a UI change:

    bash dev/shoot.sh                         # every screen -> dev/preview/.out/*.png
    bash dev/shoot.sh confirm settings:files  # just these
    SIZE=616,594 bash dev/shoot.sh main       # at the window's configured minimum

It copies `frontend/*` beside `dev/preview/stub.js` (a canned `window.__TAURI__`) and
`drive.js` (a `location.hash` director), then drives headless Chrome/Edge over the copy — the
real frontend is never touched. Screens are hashes: `main setup settings:{general,launch,files}
options confirm gd`; add one by adding a branch to `drive.js`. LAYOUT only — the stub answers
every command with fixed data; use `bun run tauri dev` to exercise the engine.
`UILANG=ru bash dev/shoot.sh …` renders the Russian tables (the long labels — check those before
calling a layout done; the stub feeds the language through `get_settings` so `boot()` agrees).
Gotchas already paid for, don't re-derive them: `--screenshot` needs an **absolute** path (a
relative one writes nothing and says nothing), each run needs its **own `--user-data-dir`** or
back-to-back launches clobber each other, and the boot reveal rides a double
`requestAnimationFrame` headless never services — `drive.js` forces `.revealed` and re-runs the
check itself, or main renders as an empty column.

    node dev/check_i18n.js      # EN/RU key parity + every $("id") exists; non-zero on a gap

**Headless engine test** (debug build keeps a console; reuses saved settings, flags override them):

    bash dev/make_decoy.sh                              # fake game folder (writes a steam.inf)
    cd src-tauri
    cargo run -- check        --game <dir> --repo <owner/name> [--token <t>]
    cargo run -- install      --game <dir> --repo <owner/name>
    cargo run -- uninstall    --game <dir>
    cargo run -- game-install --game <dir> [--game-repo <owner/name>]
    cargo run -- game-verify  --game <dir> [--game-repo <owner/name>]

A token may also be passed via `PHOENIX_GITHUB_TOKEN` (keeps it out of argv). Always test install /
uninstall against a **decoy**, never a real game install.

The CLI is **debug-only** (`#[cfg(debug_assertions)]` in `main.rs`): a release build has no console
(`windows_subsystem = "windows"`), so honouring `phoenix-launcher.exe uninstall` there would revert
a game folder silently, with no confirmation and no output. Release builds ignore argv entirely.

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
- **Compat gate — `schema`** (spec: `docs/manifest-format.md` in the dist repo): the manifest
  declares the format it is written in and the READER decides whether it can read it. The producer
  never names a launcher version — that would be a forward reference to a build that doesn't exist
  yet. (This replaced `min_launcher`, which pointed the dependency the wrong way; it is gone from
  producer and reader alike.) `manifest::{MIN,MAX}_SCHEMA` is the supported range — raise MAX in
  the same change that teaches the new format. An unsupported schema fails with a typed
  `UnsupportedSchema` (wire kind stays `tooOld`, so the UI wording is unchanged).
  **`Manifest::parse` reads `schema` in a separate permissive `Value` pass BEFORE deserializing** —
  a future manifest can carry an option `kind` we have no variant for, and parsing first would
  report "update the launcher" as an unintelligible syntax error. The `Value` is reused, so it is
  still one JSON parse. `schema` is `#[serde(skip)]` and assigned from that pass: serde's `default`
  only covers a MISSING key, so an explicit `"schema": null` would otherwise die on the second
  pass. **Absent `schema` means 1** (it predates the key). **Unknown keys are ignored everywhere** —
  the producer adds keys without bumping `schema`, so never add `deny_unknown_fields`.
- **`dest` traversal is refused at parse time** (`check_dest`, all of `files[]` / `remove[]` /
  option `dest` / option `files[]`): `dest` is joined straight onto the game dir, so it is the one
  field that turns a bad manifest into an arbitrary file write. Rejects absolute paths, `..`
  components, `:` (drive letters, NTFS streams), empty components and Windows reserved device
  names (CON/PRN/AUX/NUL/COM𝑛/LPT𝑛, any case, extension or not) — checking BOTH separators,
  since `..\` traverses on Windows just as `../` does. The device check runs on the component with
  trailing spaces/dots STRIPPED, because Win32 strips them before resolving: `"NUL "` reaches
  `\\.\NUL` (verified), so comparing the raw component let it through. The device-name check compares bytes, not
  `str` slices: `stem[..3]` panics on a multibyte boundary and file names can be Cyrillic.
- **Conformance suite**: `src-tauri/manifest-fixtures/` is the dist repo's
  `docs/manifest-fixtures/` vendored verbatim (hermetic CI — no sibling checkout needed); refresh
  by re-copying. `dist_repo_conformance_suite` walks `index.json` and asserts each documented
  expectation; the index's `schema` is asserted against `MAX_SCHEMA` so a refresh after a producer
  bump fails with the actual instruction. `ci.yml` runs `cargo test` — the contract is only a
  contract if something checks it.
- **Game-running interlock**: install and uninstall first probe every file they're about to
  touch for write access (an open without truncating). A sharing violation means the game holds
  it open (loaded DLLs, mmapped VPKs) → typed `GameRunning` error, wire kind `gameRunning`,
  before a single byte is downloaded. Any OTHER can't-write (read-only attribute, ACL) is a
  distinct permission error (wire kind `io`) — "close Dota 2" must never be the diagnosis for a
  problem closing the game can't fix. std does NOT map sharing violations to PermissionDenied —
  `is_in_use` matches the raw Windows codes (5/32/33). Install re-probes after phase 1: the game
  may have started during a long download, and commit should fail typed and untouched, not
  mid-way into a best-effort rollback.
- **Rollback records the backup BEFORE the move it protects.** `back_up` MOVES the live file out
  of the game folder, so the `Committed::Placed` entry is pushed *before* the rename that follows
  it — pushed after, a failed rename (an AV scanner holding the staged file is the everyday cause)
  rolled back a list that never mentioned the backup and the file was simply gone. Any new
  fallible step in `commit` follows the same order: record, then act.
- **`.phoenix-cache/` is the shim's; `.phoenix-cache/base/` is the game's.** They shared one
  directory until a detached `warm_cache` — which prunes against the *shim* manifest — was found
  deleting multi-GB base entries and the `.part` files an interrupted 16 GB download resumes from.
  `prune_cache` touches top-level FILES only, and uninstall clears the shim's entries with
  `clear_dir_files` rather than `remove_dir_all` for the same reason. Since nothing else may
  prune `base/`, `install_base` reclaims it itself on SUCCESS (and on the nothing-to-do path):
  every needed entry was just consumed by the placement renames, so whatever remains is stale —
  entries and `.part`s from an interrupted attempt against an older manifest, which otherwise
  sat inside the game folder forever. A cancelled/failed run keeps everything (resume sources).
- **A `.part` at or past the asset's full length is poison, not a resume source.** The Range
  request would start at EOF and GitHub answers 416 — an error, which keeps the `.part`, which
  makes the asset permanently undownloadable. Reachable without anything exotic (a completed
  transfer whose rename into the cache failed; a cancel landing on the last chunk), so
  `obtain_to_cache` drops an over-long `.part` up front and again on the error path.
- **Nothing is promoted to the vanilla store on an untrusted `prev_dests`** (`Ctx::trust_prev`).
  Without the state file we cannot tell our own installed files from genuine originals, and a
  wrong promotion is unrecoverable in effect: uninstall restores whatever is in the store as
  "stock", so the shim comes back while the UI reports the game reverted. When the state is
  missing AND the folder shows a prior install (winmm_orig.dll or a vanilla store), displaced
  files go to the ephemeral backup instead. Costs a preserved original; never fakes one.
- **Downloads**: phase 1a fetches unique-by-hash files into the content-addressed cache with an
  8-worker pool (files are independent by construction), **largest first**: the base game is
  thousands of tiny request-bound files plus a few multi-GB VPKs, so LPT scheduling keeps the
  big files streaming the whole run (alphabetical order used to leave a giant file running ALONE
  at the end) and makes the byte rate — and the ETA built on it — honest from the first seconds.
  Worker count and github.rs's POOL_PER_HOST move together, or workers churn reconnects. Staging
  copies stay sequential so commit is unchanged. Progress ticks fan out to every dest sharing an asset hash (each UI row
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
  Cancellation is an **epoch** (`WARM_EPOCH`), not a flag: a bool had to be cleared by whoever
  legitimized warming again (`install`), and that clear could un-cancel a warm the previous
  uninstall had stopped — the zombie then finished against a stale manifest and pruned what the
  new install had just seeded. A warm captures the epoch at entry and exits once it moves.
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
  The form is split into three **tabs** (General / Launch / Game files) that are DISPLAY ONLY:
  every field stays in the DOM whichever pane is shown, so the dirty-check snapshot and Save keep
  seeing one whole form and no tab can hide an unsaved edit. The active tab is remembered for the
  session (a repeat visit lands where the user was working). The Game files tab holds no form
  state at all — every control there acts immediately.
- **Manifest options (v2)**: top-level `options[]` — `kind:"choice"` (N `variants` sharing one
  `dest`, `default` = variant id) and `kind:"toggle"` (`files[]` installed when on, `default` =
  bool). Labels are plain strings or `{"en":…,"ru":…}` maps. `engine::resolve()` materializes the
  effective file set from selections; deselected files the previous install placed surface as
  `Action::Remove` (orphans) and are deleted on the next apply (vanilla originals restored). Old
  manifests without `options` parse unchanged. `gen_manifest.py` emits this (`SCHEMA = 2`); the
  authoritative spec is `docs/manifest-format.md` in the dist repo, not this file.
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
  would corrupt/blank the user's real cfg. `save_autoexec` is the backend line behind that
  read-only mode (it refuses to overwrite a non-UTF-8 file itself — the frontend must not be the
  only guard), and writes temp+rename like settings/state: it is the USER'S file, a crash
  mid-write must not truncate it.
- **Self-update** (`selfupdate.rs` + `cmd/selfupdate.rs`): the launcher replaces its own exe from
  THIS repo's Releases. Windows lets you **rename** a running exe (not delete/overwrite it), and
  the image path does not follow the rename — so: download `<stem>.new.exe` beside it → verify →
  rename running `<stem>.exe` to `<stem>.old.exe` → rename `.new` into place → spawn it (with
  `--updated`) → exit. `cleanup_old()` deletes the `.old`s at the NEXT startup, retrying ~5 s: the
  outgoing process holds its own image until it exits, so the delete cannot happen there. Each
  pass attempts EVERY leftover (a permanently locked `.old` — another copy still running — must
  not shield the rest; `.any()` short-circuited exactly that way once). A
  failed second rename rolls the first back — the running launcher must always survive.
  **Nothing is swapped unverified**: the release must publish `phoenix-launcher.exe.sha256`
  (release.yml writes it, `sha256sum` layout, ASCII so no BOM precedes the digest) and the bytes
  must match it AND start with `MZ`; a missing sidecar fails loudly. The download **never
  resumes** — a `.part` from a different version would stitch into a corrupt file of plausible
  length. Auth is **anonymous first, token retry second** (the repo is meant to be public, but a
  private repo 404s indistinguishably from a missing one, and the dist token may be scoped too
  narrowly to use unconditionally) — and the retry fires ONLY on an HTTP refusal, never on a
  transport failure: credentials can turn a 404 into a 200 but can't fix DNS, and this runs on
  the Play path where an offline user must not pay two connect timeouts. Heavyweight ops are
  serialized backend-side: `AppState::begin_op` (apply / uninstall / launcher_update / play /
  game download / repair / plan / verify — the last two write nothing but OWN the shared
  `game_cancel` flag while they run) — the
  UI's busy flag is the first line, this is the interlock behind it. `launcher_update` does the
  swap, the restart AND the exit inside its guarded closure: ending the guard at the swap left a
  window where the exe was already replaced but the event loop still lived, so an apply could
  take the freed slot and then be killed mid-commit by the exit.
  **The update installs the release the UI OFFERED**, pinned by tag (`launcher_update(tag)`), and
  refuses anything `available()` does not consider newer. Re-resolving `/releases/latest` at
  install time meant that flipping a bad release to prerelease between check and click silently
  DOWNGRADED the user, with nothing afterwards to notice it.
  **The checksum is verified against the file ON DISK**, not the download's in-memory digest —
  those are the bytes that get executed, and two launcher instances updating at once write the
  same `<stem>.new.exe` while each hashes only its own stream. A post-swap failure to relaunch
  gets its own error kind (`restartFailed`): every other failure there means "nothing was
  replaced", but this one means the new build IS installed and only needs starting.
- **Play is a gate**: pressing Play re-verifies before launching — the launcher verdict gates
  **first** (a newer release can ship a manifest format this build cannot read, so a game check
  would only dead-end on `tooOld`), then the game. Either one reporting work to do blocks the
  launch and moves the primary to that update. A pending launcher update outranks every game
  status and takes the primary button, and its release notes render in a banner on main
  (`#lu-banner`, gold-accented, shown only when the release carries notes — the status line
  already names the versions). But **can't-verify is not outdated**: when the checks fail
  for `network`/`auth`/`notFound` reasons the game still launches, with the reason in the detail
  line — Play is only reachable when the last known state was installed and clean, and neither
  GitHub being down nor a pulled/renamed release may make the game unplayable. Both `doCheck` and
  `doPlay` run the two checks concurrently (`Promise.allSettled` — Play latency is the slower
  fetch, not the sum, which matters most offline) and only *evaluate* launcher-first;
  `launcher_check` failing means *unknown*, and the frontend never collapses that into "current".
- **A folder with no game in it is SAID to have no game in it.** `CheckView.game_present`
  (game/dota exists, or an install record does) gates the whole update surface: without it a
  check of an empty folder said "Update available" and Install happily placed a shim into
  nothing. The UI shows "No game here", suppresses the file list and Customize, and the primary
  becomes the game download — **Resume download** when `pending_base_bytes` (bytes in
  `.phoenix-cache/base/`) shows an interrupted download waiting. `apply` refuses backend-side.
  A PRESENCE gate, not a build gate — the no-install-gate decision stands untouched.
  `startGameResume` is the ONE download flow that reuses the configured folder instead of
  asking: "where" was already answered by the folder holding the cache, and the confirm still
  names the exact path plus how much is already fetched (`GamePlanView.cached_bytes` +
  `cached_files`: bytes count full entries and `.part` prefixes unique-by-hash, files count
  DESTS with a complete entry only — a `.part` is byte progress, not a downloaded file;
  metadata-only, `install::base_cached`). `game_verify`'s not-a-game refusal names the
  interrupted download when one is present instead of "doesn't look like a game folder".
- **A failed check falls back to `local_check`, never to a dead end.** Play and Uninstall are
  purely local, and both are gated on `state.lastCheck`, which only a SUCCESSFUL check used to
  write — so an offline cold start left nothing but a Check button that failed again. `local_check`
  builds a CheckView from `.phoenix-state.json` alone, re-hashing each recorded dest against the
  sha256 stored at install time. It carries `local: true` and is worded "Couldn't check" in the
  error colour, never "Up to date": it knows our files are intact, it cannot know whether a newer
  release exists. It never offers `apply` (repair needs the network that just failed).
- **Base game (fresh install / verify / repair)** — `install.rs`'s base pipeline + `cmd/game.rs`:
  the base game is *just a bigger manifest* in the standard format, from `Settings::game_repo`
  (game-dist), so the same reader, schema gate, resume, hash memo and worker pool serve it. But a
  deliberately DIFFERENT pipeline from the shim flow: no staging copies, no backups, no rollback —
  files move cache→final by rename (disk transient ≈ final size, not 2×), and recovery IS
  re-running (done files hash-match and skip, `.part`s resume). It writes **NO install state**:
  the base game is not ours to uninstall. **Coexistence with the shim is by redirection**: a base
  dest whose live file the shim removed is verified/repaired at its `.phoenix-vanilla/<dest>`
  copy (repairing it live would undo the removal and re-flag it forever); a shim-managed dest
  with no vanilla copy is Skipped, never touched. Disk preflight (`free_space` via
  GetDiskFreeSpaceExW + 512 MB margin) refuses with an `io`-kind error BEFORE any bytes; the
  game-running interlock probes before and after the download. Cancellation: `game_cancel` sets
  a flag polled per chunk AND per file by `base_plan`'s hash workers → typed `Cancelled` (wire
  kind `cancelled`) → the UI closes quietly
  and a rerun resumes — the flag is reset BEFORE the blocking task is queued, since resetting it
  inside meant a Cancel clicked while the pool was busy landed first and was then wiped. The
  chained shim install takes the same flag (`install(.., cancel)`, phase 1 only — a commit must
  complete or roll back), so Cancel is not inert for that phase.
  **ONE flag covers all four base-game entry points** (plan / verify / install / repair) because
  only one can run: each resets it before queueing, the UI's busy token blocks a second flow, and
  the dialog-driven ones sit behind an inert stage. The shim's own `apply` passes `None`, so a
  stop the user asked of the game pipeline can never abort an unrelated install.
  **The hashing phase is cancellable, not just the download**: `base_plan` reads the whole install
  before a byte moves, so a Cancel that only reached the chunk loop sat inert for minutes on the
  very screen showing a Stop button. Granularity is one file per worker — a thread inside a
  multi-GB VPK finishes it — so a stop lands in seconds, not instantly. A cancelled plan returns
  the error, never its partial verdicts: every caller reads "not in the Write list" as "intact",
  so a truncated plan would be a silent all-clear. Closing the download dialog during its plan
  stage cancels it too (`gdClose`), instead of leaving the disk pinned for a number nobody
  will see.
  `game_install` chains the normal shim install and adopts the folder
  (game_dir set BEFORE the chain — a chain failure still leaves the UI pointed right, offering
  Install), then refreshes the manifest cache + warms detached, like apply. The destination is
  ALWAYS picked fresh (the download never reuses the configured game dir) and the files land
  **directly** in the picked folder — no subfolder is invented. Which is why `browse_folder` takes
  its title *and* starting directory from the CALLER: "the folder that contains `game\`" is the
  right prompt when locating an install and a lie when the folder is about to become one, and a
  picker that can't say which it means is exactly where "so where did it download to?" comes
  from. The confirm dialog then names the exact path before a byte moves. **Verify game files**
  (`game_verify`) is `base_plan` read-only — Steam-style integrity check with per-file
  `op-progress` ticks (ops: `plan` when sizing a download, `verify` when checking, `game` when
  downloading); repair is the same install run again. It is **stoppable** — minutes of hashing on
  the main view with nothing written, so quitting costs only the reading already done (the
  (size,mtime) memo keeps even that). A stop is reported in its OWN words in the neutral colour
  ("Verification stopped"), never as a verdict: the files nobody reached are exactly the ones the
  run can say nothing about. An earlier declined repair (`state.gameDamaged`) survives it, so
  Play stays blocked if it was. **`foreign_build`**: a folder whose
  `game/dota/steam.inf` EXISTS but doesn't match is a different Dota 2 build, not a damaged
  one — every file reads as "damaged" while nothing is broken, and repairing would overwrite an
  unrelated install with 1805. Verify reports it as its own outcome in the error colour and the
  confirm is worded as the overwrite it is ("Overwrite", not "Repair") — but it is NOT blocked:
  consistent with "no install gate", the folder is the user's call. An ABSENT steam.inf is a
  fresh target, not a foreign one. `BaseStatus` carries the whole
  `FileEntry`, so callers total bytes without re-resolving the manifest and scanning it per
  status (that was O(n²) over 4,635 files). UI: setup's third button, and settings' **Game files**
  tab — "Download the game" + "Verify game files" under a `6.88f` tag, alongside
  **Uninstall Phoenix**; kept off the main footer, where they crowded the everyday actions.
  The download line carries an **ETA** from a ~30 s sliding-window byte rate (frontend-only,
  `gdEta`): windowed so resume jumps and the request-bound small-file stretches don't haunt the
  estimate, repainted at most once a second, silent for the first seconds, coarse units on
  purpose ("~14 min", never "13:47"). Files-done and bytes deliberately DISAGREE mid-run — 1,290
  files can be 0.37 GB; both are true.
  Verify and Uninstall both leave settings first, through the same unsaved-changes guard as Back,
  since the status line they report through lives on main — Uninstall runs its destructive
  confirm BEFORE that guard, so declining it leaves the user where they were instead of on main.
  The 6.88f/pristine provenance is restated at every entry point (tag, settings hint, setup
  caption, download modal): "where did these gigabytes come from" must never need a guess.
- **One busy owner**: `acquireBusy()` returns a token and only that token can `releaseBusy()`. A
  bare boolean let whichever flow finished FIRST unlock the UI for both — a quick check's
  `finally` re-enabled Play in the middle of a multi-GB `game_install`. Every mutating flow
  acquires; `#btn-save` is disabled while busy for the same reason.
- **Stop is an OFFER, not a decoration**: a flow that can actually honour an interruption calls
  `offerStop(fn)`, and while the offer stands the PRIMARY button IS that Stop — ghost, not gold
  (stopping is a way out, not the recommended move), with Escape on main firing the same offer.
  No fourth footer control: the status word right above already says "Working…", so a button
  repeating it was the least useful thing on screen, and a fourth button pushed the What's-new
  chip off the edge at the minimum window size in Russian. The `ghost` class is set from ONE
  place on every render, or a finished op leaves the next Play painted in the wrong weight. The
  request LATCHES (`stopAsked`): in-flight progress ticks must not repaint "Stopping…", and a
  second press must not fire a second cancel — which is why the flow removes its `op-progress`
  listener BEFORE clearing the latch. Today `game_verify` is the only offer on main (the download
  dialog keeps its own Stop in the run stage); a Stop the backend would ignore is worse than none.
- **A modal owns the keyboard and the focus**: `.stage` is set `inert` whenever any `.modal` is
  visible (watched by a MutationObserver, so a new dialog cannot forget to do it), and the keydown
  handler ignores everything but Escape while one is open. Without this, Tab reached the view
  behind the dialog (Space could launch the game from behind a "repair?" confirm) and Enter fired
  the Enter-saves-settings branch, closing settings behind an autofind scan and discarding its
  result.
- **Verify/repair report in their OWN words** (`status.gvOk`), not the shim's "Up to date" — one
  status line was describing two different subjects, so "Up to date" could sit above "2 to
  change". Declining a repair sets `state.gameDamaged`, which blocks Play until a verify or repair
  clears it: the launcher must not offer to start a client it just called broken. **Scoped to the
  verdict, not global**: a foreign-build decline does NOT set it (verify's own verdict is that
  nothing is damaged — blocking Play painted a working install as broken with no way out short of
  overwriting it, and re-verifying re-armed the flag), and pointing at a different folder clears
  it (settings save with a changed dir, autofind adopt, fresh download) — the count described
  files in a folder the launcher no longer looks at.
- **Game tracking**: the frontend polls the `game_running` command every 3 s (a write-probe of
  the dota2.exe image via `fslock::held_by_process` — ONLY sharing/lock violations count, so a
  read-only or ACL-denied exe is never mistaken for a running game). The poll reads settings via
  `Settings::load_cached` (an mtime memo — one stat per tick instead of a full read + parse
  forever); one-shot commands stay on strict `load`, the memo is only for pollers. While the game runs the
  status shows "In game" and Play/Uninstall are disabled (the backend interlock is the second
  line); when in "check" mode the primary stays a live Check button (read-only is always
  allowed). When the game closes, one offline `replan` refreshes the status — no network, so
  closing the game while offline can't flip "Up to date" into an error (full `check` only when
  nothing is cached yet).
- **Autofind**: fast pass over Steam libraries (HKCU SteamPath + libraryfolders.vdf), then a
  bounded walk (depth 6, pruned system dirs) of all **fixed** drives (`GetDriveTypeW` —
  MAX_DEPTH bounds depth, not breadth, and there is no wall-clock budget, so a mapped network
  share turned every directory and every `steam.inf` probe into an SMB round trip and the scan
  looked hung for tens of minutes) for `game/dota/steam.inf`; progress via
  the `autofind-progress` Tauri event, cancel via AtomicBool. A cancelled scan keeps running until
  the walk notices, so results carry a sequence number — a reopened dialog never adopts them. Candidates carry their found
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
  nothing; cache stays memory + disk, keyed by the last checked tag. A FUTURE-schema release
  still contributes its entry (`version`/`notes` read permissively from the raw JSON — they are
  additive-stable): its notes are where "update the launcher" gets explained, so the history must
  not develop a hole exactly there. Truly malformed manifests are skipped, never fatal.
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
  !installed`, labeled **Repair** frontend-side). **Apply is pinned to the release the UI
  checked** (`apply(tag)` from `lastCheck.tag`, same rule as `launcher_update(tag)`): re-resolving
  "latest" at install time meant a release flipped to prerelease between check and click installed
  something the user never saw — and `install` has no newer-than gate, so possibly a downgrade.
  Removals: a file at a `remove[]`/orphan dest
  that we did NOT place is moved into `.phoenix-vanilla/` (preserved, not destroyed) and the
  removal sticks — the restore-a-vanilla-original step only fires for a copy that predates the
  removal, never for the one `back_up` just created (that would silently undo the removal and
  re-flag it on every plan, forever). **A restore is RECORDED** (`state.restored`, dests where a
  removal put a preserved original back): the file there is stock, not ours, and without the
  record `plan` saw a file at a `remove[]` dest, re-flagged it Remove, and the next apply
  displaced the restored original right back into the vanilla store — restore and removal chasing
  each other forever. `plan` skips recorded dests; the record carries through heals and drops for
  any dest a release ships (or removes) again; uninstall leaves those files where they are
  (they're stock, not in `state.files`).
- **uninstall** reverts to stock from `<game>/.phoenix-state.json`: restore a preserved vanilla
  original if one exists, else delete our file; then restore anything still left in the vanilla
  store (files preserved by removals — not in `state.files`) to its game path; delete
  `winmm_orig.dll` only if *we* created it. A corrupt state file is quarantined to
  `.phoenix-state.json.bak` on load (treated as not-installed) rather than silently misread.

## Invariants & gotchas — do not break these

- **No install gate** (removed by decision 2026-07-30): the updater installs into any chosen
  folder; wrong-build installs are the user's responsibility. The later PRESENCE gate (`apply`
  refuses a folder with no game/dota and no install record — see "no game in it" above) does not
  reverse this: it asks "is there anything here at all", never "is it the right build". (Historical note: the real 6.88
  build is ClientVersion `1805`; `6869` was a polluted value — never use it as a reference.)
- **`winmm_orig.dll`** is a copy of the system `winmm.dll`. **Never overwrite an existing one** —
  that would make the proxy's forwarders point at themselves. Uninstall deletes it only when
  `state.winmm_orig_created` is true.
- **Clean uninstall** relies on every shipped file being a net-new loose override (its stock form is
  in the VPK / System32). If a file ever shadows a genuine vanilla *loose* file, `install.rs` already
  preserves that original under `.phoenix-vanilla/` for restore — keep that path intact.
- **Releasing the launcher**: the version lives in THREE files (`src-tauri/Cargo.toml`,
  `src-tauri/tauri.conf.json`, `package.json`) and the git tag must match — self-update compares
  the tag against `CARGO_PKG_VERSION`, so a tag ahead of Cargo.toml makes every client offer an
  update that never clears itself (and the pending-update banner outranks Play, so it is
  unrecoverable without another release). `release.yml` now CHECKS all three against the tag
  before building, and publishes with `fail_on_unmatched_files` so a release can never go out
  without the `.sha256` sidecar clients require to auto-update.
- **The `.old`/`.new` exe siblings** are self-update scratch, named off the RUNNING exe's stem, so
  a launcher the user renamed still finds its own leftovers and never adopts another copy's.
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
settled/up-to-date**, **terracotta = error / irreversible**. Fonts (bundled, `@font-face`): Inter (UI) and
JetBrains Mono (paths/data). Sizing is fluid — everything is rem and the root font-size clamps on
vmin, so the whole UI scales with the window. Structural rhythm tokens (`--gap-head`, `--band`,
`--gutter`, `--marker`, `--ctl-h`, `--btn-min`) live in `:root` — retune the whole column from
there. Ring-spinner loader, staggered `.rise` reveal on main, `prefers-reduced-motion` honored.

Recurring pieces, one job each — reuse them instead of inventing a sibling:

- **`.tabs` / `.tab`** — settings' three panes. The active tab lights up the hairline it sits on
  (indicator at `bottom:-1px`); the row carries the same `--gutter` inset as `.files-head`, so the
  rule ends exactly where the scrolling pane's content does.
- **`.tag`** — a mono pill stating a FACT about the payload, never clickable (`6.88f`). The
  pristine/unmodified claim lives in the prose beside it, not in the pill.
- **`.chip`** — the What's-new pill: the only lit surface besides the primary. A sheen glints
  across it every ~9s and sweeps at once on hover (same `::after`, the hover rule replaces the
  animation) — the motion lives on the pill, not the stroked glyph, since an icon that pulses
  reads as broken. For announcements; actions stay `.btn`.
- **`.btn.danger`** — terracotta replaces gold wholesale, solid for a dialog's confirm and ghost
  for a settings row (`.btn.ghost.danger` must stay AFTER `.btn.danger` to outrank it). Reserved
  for the two irreversible acts (uninstall, overwriting a foreign build) — a red button that also
  appears for "discard my edits" stops meaning anything.
- **`.modal-actions`** — an equal-column grid: a dialog's buttons span the card edge to edge (they
  ARE its bottom rule), so no dead space can open on the right and a pair always reads as one
  unit whatever the labels say. Card width is per-modal via `--modal-w`; `.modal-text` uses
  `text-wrap: balance` so a two-line question never ends in an orphaned word.
- **`.modal-head`** — title + tag on one row. It is explicitly excluded from
  `.modal-card > div:not(.hidden)` (which makes every STAGE a flex column) — that selector
  outranks any plain `.modal-head` rule and would silently stack it.

## Editing conventions

- The **engine** (config/downloader/github/manifest/install/state/steaminf/verify/engine) stays
  UI-agnostic and pure Rust — no Tauri types in it, network only through the `Downloader` trait.
  Tauri commands live in `cmd/`, the wire contract (view structs, `CmdError`, UI-hint derivation)
  in `views.rs`.
- Settings writes go through `Settings::update` (load → mutate → save, serialized), never a
  hand-rolled load/save pair in a command.
- Prefer adding to the manifest over hardcoding in the updater. The updater should stay a dumb,
  data-driven installer.
