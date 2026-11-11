//! Windows auto-start (HKCU Run key).

#[cfg(windows)]
use anyhow::{Context, Result};
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "RapidGrab";

/// Current executable path, quoted for command-line use.
#[cfg(windows)]
pub fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("could not determine current executable path")
}

/// Command value written to the Run key.
#[cfg(windows)]
pub fn run_command(start_minimized: bool) -> Result<String> {
    let exe = exe_path()?;
    let mut cmd = format!("\"{}\"", exe.display());
    if start_minimized {
        cmd.push_str(" --minimized");
    }
    Ok(cmd)
}

/// Read the currently configured auto-start command, if any.
#[cfg(windows)]
pub fn current_command() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(RUN_KEY).ok()?;
    key.get_value::<String, _>(VALUE_NAME).ok()
}

/// Whether auto-start is enabled for the current executable (any minimized state).
#[cfg(windows)]
pub fn is_enabled() -> bool {
    let Some(cmd) = current_command() else {
        return false;
    };
    let Ok(exe) = exe_path() else {
        return false;
    };
    cmd.to_ascii_lowercase().contains(&exe.display().to_string().to_ascii_lowercase())
}

/// Enable or disable auto-start. When enabling, the command is refreshed so a
/// moved executable or changed minimized flag is corrected.
#[cfg(windows)]
pub fn set_enabled(enabled: bool, start_minimized: bool) -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY).context("could not open HKCU Run key")?;
    if enabled {
        let cmd = run_command(start_minimized)?;
        key.set_value(VALUE_NAME, &cmd)
            .context("could not write auto-start value")?;
    } else {
        match key.delete_value(VALUE_NAME) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("could not remove auto-start value"),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn set_enabled(_enabled: bool, _start_minimized: bool) -> anyhow::Result<()> {
    anyhow::bail!("auto-start is only supported on Windows")
}

#[cfg(not(windows))]
pub fn current_command() -> Option<String> {
    None
}
