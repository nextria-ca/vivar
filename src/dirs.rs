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
