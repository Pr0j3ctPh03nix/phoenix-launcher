//! Small shell utilities: open external links, pick a folder.

use crate::views::CmdError;

/// Open an http(s) link in the default browser (changelog links must not navigate the webview).
#[tauri::command]
pub fn open_url(url: String) -> Result<(), CmdError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CmdError::from("only http(s) links can be opened"));
    }
    // `explorer <url>` hands it to the default browser without flashing a console window
    std::process::Command::new("explorer")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| CmdError::from(format!("opening {url}: {e}")))
}

#[tauri::command]
pub fn browse_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Select the game folder (the one that contains game\\)")
        .pick_folder()
        .map(|p| p.display().to_string())
}
