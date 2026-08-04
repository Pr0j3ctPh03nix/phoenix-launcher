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

#[tauri::command]
pub fn browse_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Select the game folder (the one that contains game\\)")
        .pick_folder()
        .map(|p| p.display().to_string())
}
