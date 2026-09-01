//! Download-source commands: the settings pane's writer, and the sweeps.
//!
//! INSTANT-APPLY, unlike the rest of settings — the per-mirror switch writes through on the spot.
//! That keeps sources out of the unsaved-form snapshot entirely, and it is what lets a sweep
//! persist a re-ordered list without reconciling it against a half-edited form.
//!
//! There is no add and no remove: mirrors are DISCOVERED from the published `mirrors.json`, so the
//! only thing a user decides about one is whether to use it. The primary source is not in that
//! document and has no switch, which is what makes it impossible for any published list to take
//! the main source away.

use crate::config::{Settings, Source, SourceRef};
use crate::mirror;
use crate::views::{CmdError, MirrorSweepView};

/// Enable or disable one discovered mirror. Keyed by URL because that is a mirror's identity, and
/// because a list that a sweep reorders underneath the UI has no stable index.
#[tauri::command]
pub fn set_mirror_enabled(url: String, enabled: bool) -> Result<(), CmdError> {
    Settings::update(move |s| {
        for src in &mut s.sources {
            if let Source::Mirror { url: u, enabled: e, .. } = src {
                if *u == url {
                    *e = enabled;
                }
            }
        }
    })
    .map_err(CmdError::from)
}

/// Pin a source, or pass `null` to go back to following the ranking.
///
/// Selecting a switched-off mirror turns it on: the alternative is a click that visibly does
/// nothing, because a disabled source can never be the active one.
#[tauri::command]
pub fn set_selected_source(selected: Option<SourceRef>) -> Result<(), CmdError> {
    Settings::update(move |s| {
        if let Some(r) = &selected {
            for src in &mut s.sources {
                if let (Source::Mirror { url, enabled, .. }, SourceRef::Mirror { url: pinned }) =
                    (&mut *src, r)
                {
                    if url == pinned {
                        *enabled = true;
                    }
                }
            }
        }
        s.selected = selected;
    })
    .map_err(CmdError::from)
}

/// The button: refresh the published list, time every source, order them fastest-first, and drop
/// any pin so the fastest is selected again.
///
/// Read-only as far as the game folder goes, so it claims no `begin_op` slot (see `AppState`); the
/// frontend disables its own button for the duration. A refresh failure is not fatal — the sweep
/// measures whatever list was already there and reports why it could not be updated.
#[tauri::command]
pub async fn sweep_mirrors() -> Result<MirrorSweepView, CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        let sweep = mirror::sweep(&Settings::load(), true);
        // `persist` writes the measured order AND the accepted list's serial together — see
        // `mirror::Refresh::persist`. `true`: asking to be re-tested is asking for the answer the
        // test gives, so the pin goes.
        sweep.persist(true).map_err(CmdError::from)?;
        Ok(MirrorSweepView::build(sweep, None))
    })
    .await
    .map_err(CmdError::task)?
}

/// Whether a new mirror triggers an automatic test-and-switch (applies instantly, like the rest
/// of this pane).
#[tauri::command]
pub fn set_auto_pick_best(on: bool) -> Result<(), CmdError> {
    Settings::update(move |s| s.auto_pick_best = on).map_err(CmdError::from)
}

/// The launch-time pass, fired once at boot and never awaited by the UI.
///
/// Refreshing the LIST is cheap — one small document — so it happens every launch, which is how a
/// newly published mirror reaches users who never open this pane. MEASURING is on no schedule: it
/// costs a real download per source, and re-ordering unprompted is what would move someone off the
/// source they chose. It runs only when the refresh turned up a mirror nobody has timed — which
/// can only be asked after refreshing, and is why this is not one call — and only when
/// `auto_pick_best` is on. With it off, a new mirror is listed as untested and nothing else
/// happens until the user asks.
///
/// When it does run it switches to the best, pin included: that is what the setting promises. The
/// one exception is a measurement in which NOTHING was usable — see `mirror::any_healthy`.
#[tauri::command]
pub async fn auto_sweep_mirrors() -> Result<(), CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let refreshed = mirror::refresh(&settings);

        // MEASURING is gated on a mirror actually being usable as a download source, which today
        // it is not: `MIRROR_DOWNLOADS_ENABLED` is the deployment switch (read its doc), and while
        // it is off every installed byte comes from GitHub regardless of how this ranks. Until it
        // flips, a boot-time sweep spends a 512 KiB transfer and up to ~8s PER SOURCE, on every
        // launch, to order a list nothing reads. Refreshing the list is still worth doing — it is
        // one request and it is what surfaces a newly published mirror in the settings pane.
        //
        // This early return is therefore the branch that runs on EVERY launch today, which makes it
        // the one place the mirror serial ratchet actually advances in the field. `persist` is what
        // carries it; a bare `s.sources = …` here would leave the anti-rollback floor at zero
        // forever, and nothing would ever look wrong.
        if !mirror::MIRROR_DOWNLOADS_ENABLED
            || !settings.auto_pick_best
            || !mirror::has_new_mirror(&refreshed.sources)
        {
            return refreshed.persist(false).map_err(CmdError::from);
        }

        let (refreshed, probes) = refreshed.measured(&settings);
        // Follow the ranking, whose head is now the fastest that works — unless NOTHING was usable.
        let pick_best = mirror::any_healthy(&probes);
        refreshed.persist(pick_best).map_err(CmdError::from)
    })
    .await
    .map_err(CmdError::task)?
}
