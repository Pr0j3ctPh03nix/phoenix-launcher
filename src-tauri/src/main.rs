#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Tauri desktop app. The window/UI is HTML/CSS/JS under ../frontend; the engine
//! (config/downloader/github/manifest/install/state/steaminf/verify/engine) is UI-agnostic pure
//! Rust. This binary is only wiring: the command layer (cmd/*), the webview wire contract
//! (views.rs), and a headless CLI (cli.rs) for testing in debug builds.
//!
//! i18n note: user-facing labels are derived in the frontend (it owns the language); the shell
//! ships raw data + minimal hints (`primary_action`, `can_play`, …). Manifest labels pass through
//! as plain strings or `{lang: text}` objects for the frontend to resolve.

mod autofind;
mod cli;
mod cmd;
mod config;
mod downloader;
mod engine;
mod fslock;
mod github;
mod install;
mod launch;
mod manifest;
mod state;
mod steaminf;
mod verify;
mod views;

use std::sync::Arc;
use tauri_plugin_window_state::StateFlags;

fn run_gui() {
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
            cmd::settings::set_selection,
            cmd::settings::game_dir_status,
            cmd::update::check,
            cmd::update::replan,
            cmd::update::apply,
            cmd::update::uninstall,
            cmd::notes::release_notes,
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("check") => cli::run_check(&args[1..]),
        Some("install") => cli::run_install(&args[1..]),
        Some("uninstall") => cli::run_uninstall(&args[1..]),
        _ => {
            run_gui();
            return;
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
