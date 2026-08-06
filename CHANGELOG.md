# Changelog

## 1.2.1 — 2026-08-06

### Added
- **Launch tweaks**: settings expose optional launch options as switches, the first being
  *Keep keybinds local* (`+dota_keybindings_cloud_disable 1`) — Dota otherwise syncs
  keybindings through Steam Cloud and can overwrite local ones on launch.
  `launch::LAUNCH_FLAGS` (id + args + default) is the single source of truth: the settings
  view, `save_settings` and the spawn all read it, so a new flag is one table row plus a
  `set.flag.<id>` string (without one the UI shows the raw args). Persisted as
  `launch_flags` (id -> bool), storing only ids the table knows — a missing id means the
  flag's own default, so new flags need no migration and a stale key from another build
  cannot inject arguments. The user's extras still come last, so a duplicated option lands
  on their value.

### Fixed
- **Release builds no longer recompile every dependency from scratch.** Actions caches are
  ref-scoped: a run may only restore caches from its own ref or from the default branch, so
  the cache each tag build saved was invisible to every later tag — `release` never once hit
  its own cache. A new `ci` workflow (main pushes, weekly cron, manual) now writes the cache
  from `main` and `release` is restore-only; both carry the same rust-cache `shared-key`,
  since the default key embeds the job id and would differ across workflows. The weekly run
  also keeps the entry ahead of GitHub's 7-day eviction of unused caches.

## 1.2.0 — 2026-08-04

### Added
- **Cache warm**: after a successful apply, unselected manifest assets (disabled
  toggles, unchosen variants) are prefetched on a detached task so customization flips
  never wait on the network. It never blocks Play; uninstall cancels it, with an
  in-flight hash guard against a concurrent apply.
- **Live game tracking**: a new `fslock` module (writable / held-by-process split) backs
  a `game_running` command; the frontend polls it every 3 s and shows an "In game"
  status with Play/Uninstall disabled. Closing the game triggers an offline replan (no
  network), so it can never flip "Up to date" into an error.
- **Window-state persistence**: window position and maximized state persist across runs;
  first run centers the window.

### Changed
- **Per-file progress**: download ticks fan out to every destination sharing an asset
  hash, so each UI row gets its own bar; `OpProgress.done` settles bars cleanly under
  interleaved parallel downloads.
- **Faster history rebuild**: unseen release manifests fetch through a 4-worker pool
  instead of N serial round trips.
- **Interruptible downloads**: the first download failure aborts sibling in-flight
  streams at their next chunk (`ChunkProgress -> bool`) rather than waiting on a huge
  neighbor; `cancel_warm` also aborts mid-file.
- **Offline post-op refreshes**: `apply` refreshes the manifest cache from the release
  it actually installed (`InstallReport` carries the manifest + tag), so post-apply and
  post-uninstall refreshes are offline-safe replans.
- **Kind-aware error status**: the status word is now specific to the failure —
  Offline / No access / Launcher outdated / Game is running.
- `sha256` now reads 256 KiB blocks; `open_url` goes through `ShellExecuteW` for a real
  failure signal (no `explorer.exe` hand-off).

### Fixed
- **Removal loop**: a `remove[]` entry pointing at a file we did not place is now
  preserved into `.phoenix-vanilla/` and the removal *sticks* — the old path restored the
  just-preserved copy in the same breath, re-flagging it as `Remove` on every plan
  forever. Uninstall now restores any files left in the vanilla store.
- **Honest lock diagnosis**: `fslock::probe()` is three-way — only a sharing violation
  maps to `GameRunning`; a read-only attribute or ACL denial gets a permission error
  instead of a misleading "close Dota 2".
- **Autoexec safety**: a non-UTF-8 cfg comes back lossy-decoded and flagged, and the
  editor goes read-only — a lossy save would corrupt the real bytes. A failed read no
  longer self-clobbers its own error; Save closes the editor on success.
- **Stale views**: a settings save that changes the game folder, repo, or token
  invalidates the check view and re-checks, so Play can no longer act on a folder the
  visible status doesn't describe.
- **Autofind**: double-start guard, fixed listener leak, and a failed scan now shows why
  instead of masquerading as "Nothing found".
- **Interlock re-probe** after phase 1: a game started mid-download fails typed and
  untouched instead of mid-rollback.
- **Quit guard**: added `core:window:allow-destroy` (not part of `core:default`) —
  without it a JS `onCloseRequested` listener made the window's X silently dead. Closing
  while an op runs now confirms first.
- Atomic (temp+rename) state/settings saves; the saved token is clearable via an explicit
  flag; the Repair (heal) case is worded correctly and the download counter counts
  destinations to match the visible rows.

## 1.1.0

- Downloader seam, self-healing installs, game-running interlock, parallel resumable
  downloads. Persisted what's-new history; retuned layout; faster builds. CI cancels
  superseded release runs and keeps rust-cache on failed builds.

## 1.0.1

- Maintenance release.

## 1.0.0

- Initial release.
