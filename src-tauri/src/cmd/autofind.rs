//! Autofind commands: start/cancel the machine-wide game-folder scan, with progress events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::Emitter;

use crate::autofind;
use crate::cmd::AppState;
use crate::views::{CandidateView, CmdError};

/// Resets `autofind_running` when the scan closure exits — including via panic, so a crashed
/// scan can never block every future scan with "already running".
struct ClearOnDrop<'a>(&'a AtomicBool);
impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tauri::command]
pub async fn autofind_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<CandidateView>, CmdError> {
    let st = state.inner().clone();
    // one scan at a time — a double-fired Continue must not spawn a second disk walk
    if st.autofind_running.swap(true, Ordering::SeqCst) {
        return Err(CmdError::from("a scan is already running"));
    }
    st.autofind_cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let _running = ClearOnDrop(&st.autofind_running);
        let found = autofind::autofind(
            |p| {
                let _ = app.emit(
                    "autofind-progress",
                    serde_json::json!({ "scanned": p.scanned, "current": p.current, "found": p.found }),
                );
            },
            &st.autofind_cancel,
        );
        Ok(found
            .into_iter()
            .map(|c| CandidateView {
                path: c.path.display().to_string(),
                client_version: c.client_version,
            })
            .collect())
    })
    .await
    .map_err(CmdError::task)?
}

#[tauri::command]
pub fn autofind_cancel(state: tauri::State<'_, Arc<AppState>>) {
    state.autofind_cancel.store(true, Ordering::Relaxed);
}
