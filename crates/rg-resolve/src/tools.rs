//! Locating and invoking the external helper tools.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::{ResolveError, Result};

/// Paths to the optional helpers. `yt-dlp` is required for resolution;
/// `ffmpeg` is required only when a separate audio track must be merged.
#[derive(Debug, Clone)]
pub struct Tools {
    pub yt_dlp: PathBuf,
    pub ffmpeg: Option<PathBuf>,
}

impl Tools {
    /// Find both tools on `PATH` and in common install locations.
    pub fn detect() -> Result<Self> {
        let yt_dlp = find_tool("yt-dlp").ok_or(ResolveError::YtDlpMissing)?;
        let ffmpeg = find_tool("ffmpeg");
        Ok(Self { yt_dlp, ffmpeg })
    }

    /// Use explicit settings when provided, otherwise auto-detect.
    ///
    /// An explicitly configured path must exist: silently falling back would
    /// hide a typo in Settings.
    pub fn resolve_paths(yt_dlp: Option<PathBuf>, ffmpeg: Option<PathBuf>) -> Result<Self> {
        let yt_dlp = match yt_dlp {
            Some(path) if path.is_file() => path,
            Some(path) => {
                return Err(ResolveError::BadToolPath {
                    tool: "yt-dlp",
                    path: path.display().to_string(),
                })
            }
            None => find_tool("yt-dlp").ok_or(ResolveError::YtDlpMissing)?,
        };
        let ffmpeg = match ffmpeg {
            Some(path) if path.is_file() => Some(path),
            Some(path) => {
                return Err(ResolveError::BadToolPath {
                    tool: "ffmpeg",
                    path: path.display().to_string(),
                })
            }
            None => find_tool("ffmpeg"),
        };
        Ok(Self { yt_dlp, ffmpeg })
    }
}

/// Search `PATH`, then known install directories (pip --user, WinGet, Scoop,
/// Chocolatey, and the Python launcher directories).
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let names: Vec<OsString> = if cfg!(windows) {
        vec![format!("{name}.exe").into(), name.into()]
    } else {
        vec![name.into()]
    };

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for candidate in &names {
                let full = dir.join(candidate);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
    }

    for dir in extra_dirs() {
        for candidate in &names {
            let full = dir.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    fn push(dirs: &mut Vec<PathBuf>, key: &str, suffix: &str) {
        if let Some(base) = std::env::var_os(key) {
            let mut path = PathBuf::from(base);
            for part in suffix.split('\\').filter(|p| !p.is_empty()) {
                path.push(part);
            }
            dirs.push(path);
        }
    }

    // pip --user installs scripts here; scan every Python3xx directory.
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let python_root = PathBuf::from(appdata).join("Python");
        if let Ok(entries) = std::fs::read_dir(&python_root) {
            for entry in entries.flatten() {
                let scripts = entry.path().join("Scripts");
                if scripts.is_dir() {
                    dirs.push(scripts);
                }
            }
        }
        dirs.push(python_root.join("Scripts"));
    }

    push(
        &mut dirs,
        "LOCALAPPDATA",
        "Programs\\Python\\Python314\\Scripts",
    );
    push(&mut dirs, "LOCALAPPDATA", "Microsoft\\WinGet\\Links");
    push(&mut dirs, "LOCALAPPDATA", "Programs\\ffmpeg\\bin");
    push(&mut dirs, "USERPROFILE", "scoop\\shims");
    push(&mut dirs, "ProgramData", "chocolatey\\bin");
    if cfg!(windows) {
        dirs.push(PathBuf::from("C:\\ffmpeg\\bin"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        dirs.push(PathBuf::from(program_files).join("ffmpeg").join("bin"));
    }

    // WinGet installs portable packages under
    // `%LOCALAPPDATA%\Microsoft\WinGet\Packages\<id>\<version>\bin` and does
    // not always create a Links shim, so scan one level for `bin` directories.
    if let Some(localappdata) = std::env::var_os("LOCALAPPDATA") {
        let packages = PathBuf::from(localappdata)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages");
        if let Ok(entries) = std::fs::read_dir(&packages) {
            for entry in entries.flatten() {
                let package = entry.path();
                if !package.is_dir() {
                    continue;
                }
                dirs.push(package.join("bin"));
                if let Ok(children) = std::fs::read_dir(&package) {
                    for child in children.flatten() {
                        if child.path().is_dir() {
                            dirs.push(child.path().join("bin"));
                        }
                    }
                }
            }
        }
    }

    dirs
}

/// Result of one helper invocation.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub success: bool,
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn stderr_text(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let text = text.trim();
        if text.len() > 2_000 {
            format!("…{}", &text[text.len() - 2_000..])
        } else {
            text.to_string()
        }
    }
}

/// Indirection that lets tests substitute fake helper binaries.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, program: &Path, args: &[String]) -> Result<CommandOutput>;
}

/// Runs real child processes without a shell.
pub struct SystemRunner;

#[async_trait]
impl Runner for SystemRunner {
    async fn run(&self, program: &Path, args: &[String]) -> Result<CommandOutput> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|source| ResolveError::Spawn {
                tool: program.display().to_string(),
                source,
            })?;

        Ok(CommandOutput {
            success: output.status.success(),
            status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_real_system_tool() {
        // `cmd` exists on every Windows install; the test is about PATH lookup,
        // not about the specific tool.
        if cfg!(windows) {
            assert!(find_tool("cmd").is_some(), "cmd.exe should be on PATH");
        }
    }
}
