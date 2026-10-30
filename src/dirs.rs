//! Where vivar keeps its own state. XDG on Unix, the usual folders on Windows.

use anyhow::{anyhow, Result};
use std::env;
use std::path::PathBuf;

fn home() -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(var)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{var} is not set"))
}

fn xdg(var: &str, fallback: &[&str]) -> Result<PathBuf> {
    if let Some(dir) = env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(dir);
    }
    let mut dir = home()?;
    dir.extend(fallback);
    Ok(dir)
}

/// The machine-wide data folder (`C:\ProgramData`), asked of the shell so a
/// caller's `ProgramData` variable can't point policy somewhere else.
#[cfg(windows)]
pub fn program_data() -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);
    #[link(name = "shell32")]
    extern "system" {
        fn SHGetKnownFolderPath(
            rfid: *const Guid,
            flags: u32,
            token: *mut core::ffi::c_void,
            path: *mut *mut u16,
        ) -> i32;
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(p: *mut core::ffi::c_void);
    }
    // FOLDERID_ProgramData.
    const PROGRAM_DATA: Guid = Guid(
        0x62AB5D82,
        0xFDC1,
        0x4DC3,
        [0xA9, 0xDD, 0x07, 0x0D, 0x1D, 0x49, 0x5D, 0x97],
    );
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: the folder id is a valid GUID, `out` receives a buffer the
    // shell allocates and that is freed here, after it is copied out.
    unsafe {
        if SHGetKnownFolderPath(&PROGRAM_DATA, 0, std::ptr::null_mut(), &mut out) >= 0
            && !out.is_null()
        {
            let len = (0..).take_while(|&i| *out.add(i) != 0).count();
            let path = OsString::from_wide(std::slice::from_raw_parts(out, len));
            CoTaskMemFree(out.cast());
            return PathBuf::from(path);
        }
    }
    PathBuf::from(r"C:\ProgramData")
}

/// Tool environments and other long-lived state.
pub fn data() -> Result<PathBuf> {
    if cfg!(windows) {
        return xdg("APPDATA", &["AppData", "Roaming"]).map(|d| d.join("vivar"));
    }
    xdg("XDG_DATA_HOME", &[".local", "share"]).map(|d| d.join("vivar"))
}

/// Script environments and other things safe to delete.
pub fn cache() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("VIVAR_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if cfg!(windows) {
        return xdg("LOCALAPPDATA", &["AppData", "Local"]).map(|d| d.join("vivar").join("cache"));
    }
    xdg("XDG_CACHE_HOME", &[".cache"]).map(|d| d.join("vivar"))
}

/// Per-user configuration (the org policy file lives here by default).
pub fn config() -> Result<PathBuf> {
    if cfg!(windows) {
        return xdg("APPDATA", &["AppData", "Roaming"]).map(|d| d.join("vivar"));
    }
    xdg("XDG_CONFIG_HOME", &[".config"]).map(|d| d.join("vivar"))
}

/// Where `tool install` links entry points: `~/.local/bin` on every platform,
/// the same place the installers put vivar.
pub fn bin() -> Result<PathBuf> {
    Ok(home()?.join(".local").join("bin"))
}
