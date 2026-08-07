# Known bugs / accepted residual risks

Issues that were found, understood, and deliberately left. Each entry says what triggers it,
why it is accepted, and the fix direction if it ever stops being acceptable. Reviewed 2026-08-07.

## Cancel-flag entry reset can wipe another op's pending cancel

`game_plan` / `game_verify` / `game_install` / `game_repair` share one `game_cancel` flag and
each RESETS it synchronously at command entry — before `begin_op` can refuse the newcomer.
Moving the reset inside the guarded closure is the documented lost-Cancel-click bug (a cancel
clicked while the blocking pool was busy landed first and was wiped), so the reset must stay at
entry — which means a second op invoked while one runs wipes the running op's pending cancel
even though its own `begin_op` then fails.

**Why accepted:** reaching it requires the shipped frontend to misbehave — every entry point is
behind the UI busy token, and (since the same review) all four ops also hold the backend
`begin_op` slot. **Fix direction:** a cancel *generation* counter (the `WARM_EPOCH` pattern in
install.rs) instead of a shared bool; each op captures its generation at entry, `game_cancel`
bumps it.

## Detached cache warm can mismatch a pinned apply

`apply` installs the release the UI checked (pinned by tag), but the detached `warm_cache` it
triggers refetches `/releases/latest`. If a new release lands between check and click, the warm
prefetches and prunes against the NEWER manifest than the one just installed — possibly evicting
cached assets the installed release references.

**Why accepted:** the window is one release landing inside a single check→apply session; warm is
best-effort by design, and the worst case is a re-download when a customization toggle flips.
**Fix direction:** pass the installed release's tag into `warm_cache`.

## After a pinned apply the UI says "Up to date" against a possibly stale release

Pinning means: check sees v5 → v6 publishes → user clicks Update → v5 installs (correctly — it
is what the button offered) → the post-apply replan diffs against v5's manifest and reports
"Up to date" until the next check finds v6.

**Why accepted:** inherent to pinning (the self-update path has the same shape), and the
alternative — silently installing something the user never saw, possibly a downgrade — was the
actual bug. Any check refreshes it.

## Base-game cache stranded in an abandoned target folder

`install_base` reclaims `.phoenix-cache/base/` only when a run COMPLETES in that folder (the
leftovers are otherwise resume sources). A download cancelled in folder A and never resumed —
because the user downloaded into folder B instead, or gave up — leaves up to ~16 GB in A that
nothing ever deletes.

**Why accepted:** the launcher cannot safely reach into folders it is no longer pointed at, and
deleting resume state on any weaker signal than a completed run would break download resumption.
**Fix direction (UI):** surface interrupted-download data in the Game files tab with an explicit
delete action when the configured game dir contains one.
