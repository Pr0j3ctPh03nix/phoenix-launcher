//! Small shell utilities: open external links, pick a folder.

use crate::views::CmdError;

/// Open an http(s) link in the default browser (changelog links must not navigate the webview).
#[tauri::command]
pub fn open_url(url: String) -> Result<(), CmdError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CmdError::from("only http(s) links can be opened"));
    }
    // ShellExecuteW "open": the canonical default-browser hand-off — no console flash, no shell
    // re-parsing of the URL, and (unlike spawning explorer.exe) an actual failure signal.
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> =
        std::ffi::OsStr::new(&url).encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let r = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // Win32 legacy contract: an HINSTANCE value > 32 means success
    if r as usize <= 32 {
        return Err(CmdError::from(format!("opening {url} failed (ShellExecute code {})", r as usize)));
    }
    Ok(())
}

/// Pick a folder. The caller supplies the dialog title because the two uses mean opposite things:
/// LOCATING an existing install ("the folder that contains game\") vs choosing a DESTINATION for a
/// fresh download (a folder that contains nothing yet) — one title cannot be honest about both.
/// It comes from the frontend since that is what owns the language; `start` pre-selects a folder
/// (the current game dir) so the dialog opens somewhere useful instead of at the last shell path.
#[tauri::command]
pub fn browse_folder(title: Option<String>, start: Option<String>) -> Option<String> {
    let mut d = rfd::FileDialog::new()
        .set_title(title.as_deref().unwrap_or("Select a folder"));
    if let Some(s) = start.filter(|s| !s.is_empty()) {
        let p = std::path::PathBuf::from(s);
        if p.is_dir() {
            d = d.set_directory(p);
        }
    }
    d.pick_folder().map(|p| p.display().to_string())
}
