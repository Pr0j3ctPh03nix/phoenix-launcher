#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Tauri desktop app. The window/UI is HTML/CSS/JS under ../frontend; the engine
//! (config/downloader/github/manifest/install/state/steaminf/verify/engine, plus
//! keep/trust/selfupdate/mirror/launch/autofind/fslock) is UI-agnostic pure Rust. This binary is
//! only wiring: the command layer (cmd/*), the webview wire contract (views.rs), and a headless
//! CLI (cli.rs) for testing in debug builds.
//!
//! i18n note: user-facing labels are derived in the frontend (it owns the language); the shell
//! ships raw data + minimal hints (`primary_action`, `can_play`, …). Manifest labels pass through
//! as plain strings or `{lang: text}` objects for the frontend to resolve.

mod autofind;
// Debug-only like its dispatcher below: gated at the MODULE, not just the call site, so release
// builds neither ship the CLI code nor warn about it being dead (nothing references it there).
#[cfg(debug_assertions)]
mod cli;
mod cmd;
mod config;
mod downloader;
mod engine;
mod fslock;
mod github;
mod install;
mod keep;
mod launch;
mod manifest;
// The `.minisig` format, split out of trust.rs because `build.rs` includes this same file —
// see its own module doc.
mod minisig;
mod mirror;
mod selfupdate;
mod state;
mod steaminf;
// Test-only shared HTTP/1.1 test server for github.rs/mirror.rs redirect-chain unit tests — see
// its own doc comment for why a real TCP round trip is what those tests need.
#[cfg(test)]
mod test_http;
mod transport;
mod trust;
mod verify;
mod views;

use std::sync::Arc;
use tauri_plugin_window_state::StateFlags;

fn run_gui() {
    // Collect the previous binary if a self-update just restarted us. Detached and best-effort:
    // the outgoing process may still hold its image, so this retries in the background.
    selfupdate::cleanup_old();
    tauri::Builder::default()
        // remember where the window was: position + maximized (the plugin itself validates the
        // saved spot against the connected monitors and lets the OS decide if it's gone).
        // Never SIZE (every run starts at the config size unless maximized) and never VISIBLE
        // (hidden-until-first-paint is managed by the frontend).
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::POSITION | StateFlags::MAXIMIZED)
                .build(),
        )
        .manage(Arc::new(cmd::AppState::default()))
        .invoke_handler(tauri::generate_handler![
            cmd::settings::get_settings,
            cmd::settings::save_settings,
            cmd::settings::set_game_dir,
            cmd::settings::set_language,
            cmd::settings::set_animations,
            cmd::settings::set_selection,
            cmd::settings::game_dir_status,
            cmd::mirrors::set_mirror_enabled,
            cmd::mirrors::set_selected_source,
            cmd::mirrors::set_auto_pick_best,
            cmd::mirrors::sweep_mirrors,
            cmd::mirrors::auto_sweep_mirrors,
            cmd::update::check,
            cmd::update::local_check,
            cmd::update::replan,
            cmd::update::apply,
            cmd::update::uninstall,
            cmd::game::game_target,
            cmd::game::game_plan,
            cmd::game::game_install,
            cmd::game::game_repair,
            cmd::game::game_verify,
            cmd::game::your_files,
            cmd::game::game_delete_extras,
            cmd::game::phoenix_keep,
            cmd::game::game_cancel,
            cmd::notes::release_notes,
            cmd::notes::launcher_notes,
            cmd::selfupdate::launcher_check,
            cmd::selfupdate::launcher_update,
            cmd::selfupdate::launcher_info,
            cmd::launch::play,
            cmd::launch::game_running,
            cmd::launch::read_autoexec,
            cmd::launch::save_autoexec,
            cmd::autofind::autofind_start,
            cmd::autofind::autofind_cancel,
            cmd::misc::open_url,
            cmd::misc::browse_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running the updater");
}

fn main() {
    // The CLI is an engine test harness (`cargo run -- check …`), and it is DEBUG-ONLY on purpose:
    // release builds are `windows_subsystem = "windows"` and have no console, so a shipped exe
    // that honoured `phoenix-launcher.exe uninstall` would revert someone's game folder silently,
    // with no confirmation and no output. Release builds ignore argv and open the window.
    #[cfg(debug_assertions)]
    if let Some(r) = cli_dispatch() {
        if let Err(e) = r {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    run_gui();
}

#[cfg(debug_assertions)]
fn cli_dispatch() -> Option<anyhow::Result<()>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    Some(match args.first().map(String::as_str) {
        Some("check") => cli::run_check(&args[1..]),
        Some("install") => cli::run_install(&args[1..]),
        Some("uninstall") => cli::run_uninstall(&args[1..]),
        Some("game-install") => cli::run_game_install(&args[1..]),
        Some("game-verify") => cli::run_game_verify(&args[1..]),
        Some("sweep") => cli::run_sweep(&args[1..]),
        _ => return None, // not a CLI invocation — the caller opens the window
    })
}
