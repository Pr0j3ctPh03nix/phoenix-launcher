//! The download-source status block: one read-only command, and the view it hands the webview.
//!
//! There is nothing to write here. Sources are DISCOVERED from the published `mirrors.json` and
//! ranked by a real measurement, so there is no setting to save, nothing to switch off and nothing
//! to pin — every control the old mirrors pane had described a decision the user has no information
//! to make and would then be stuck with when the host it named went dark. What is left is what the
//! ranking currently says, which is worth SHOWING and nothing else.

use crate::source;
use crate::views::SourcesView;

/// The registry as it stands. Synchronous and network-free: it reads process state.
///
/// Kept even though the shell PUSHES changes (`sources-changed`), for the two moments a push cannot
/// serve — the first paint, and a webview that reloaded and missed the events it was not there for.
#[tauri::command]
pub fn download_sources() -> SourcesView {
    view()
}

/// The same value, for the change sink `main.rs` installs. The command and the event carry exactly
/// one shape, so a frontend that got there by either route is looking at the same thing.
pub fn view() -> SourcesView {
    SourcesView::of(source::snapshot())
}
