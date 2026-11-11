//! Virus-check hook: run an external scanner against a completed download.
//!
//! The command template may contain `{file}` which is replaced with the
//! quoted full path. If `{file}` is absent, the path is appended as the last
//! argument. Off by default.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirusStatus {
    Clean,
    Threat,
    Error,
}

impl VirusStatus {
    pub fn label(&self) -> &'static str {
        match self {
            VirusStatus::Clean => "Clean",
            VirusStatus::Threat => "Threat found",
            VirusStatus::Error => "Scan error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VirusResult {
    pub status: VirusStatus,
    pub message: String,
}

/// Split a command template into program + args, substituting `{file}`.
pub fn build_command(template: &str, file: &Path) -> Option<(String, Vec<String>)> {
    let file_str = file.display().to_string();
    let substituted = if template.contains("{file}") {
        template.replace("{file}", &file_str)
    } else {
        format!("{} \"{}\"", template.trim(), file_str)
    };
    let parts = crate::args::split_args(&substituted);
    let mut it = parts.into_iter();
    let program = it.next()?;
    if program.is_empty() {
        return None;
    }
    Some((program, it.collect()))
}

/// Classify a scanner exit code. Convention: 0 = clean, 2 = threat (Defender).
pub fn classify_exit(code: Option<i32>, stderr: &str, stdout: &str) -> VirusStatus {
    match code {
        Some(0) => VirusStatus::Clean,
        Some(2) => VirusStatus::Threat,
        Some(_) => {
            let text = format!("{} {}", stdout, stderr).to_ascii_lowercase();
            if text.contains("threat") || text.contains("infect") || text.contains("malware") {
                VirusStatus::Threat
            } else {
                VirusStatus::Error
            }
        }
        None => VirusStatus::Error,
    }
}

fn truncate_output(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Run the configured scanner. Intended to be called from a background thread.
pub fn run_scan(template: &str, file: &Path) -> VirusResult {
    let Some((program, args)) = build_command(template, file) else {
        return VirusResult {
            status: VirusStatus::Error,
            message: "Empty virus-check command".to_string(),
        };
    };
    if !file.exists() {
        return VirusResult {
            status: VirusStatus::Error,
            message: format!("File not found: {}", file.display()),
        };
    }

    let start = Instant::now();
    let mut child = match Command::new(&program)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return VirusResult {
                status: VirusStatus::Error,
                message: format!("Could not start scanner '{}': {e}", program),
            }
        }
    };

    let timeout = Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return VirusResult {
                        status: VirusStatus::Error,
                        message: "Virus scan timed out after 60 s".to_string(),
                    };
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return VirusResult {
                    status: VirusStatus::Error,
                    message: format!("Scanner wait failed: {e}"),
                }
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return VirusResult {
                status: VirusStatus::Error,
                message: format!("Could not read scanner output: {e}"),
            }
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let status = classify_exit(output.status.code(), &stderr, &stdout);
    let detail = if !stdout.trim().is_empty() {
        stdout.trim()
    } else if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        ""
    };
    let message = if detail.is_empty() {
        match status {
            VirusStatus::Clean => "No threats found".to_string(),
            VirusStatus::Threat => "Threat detected".to_string(),
            VirusStatus::Error => format!("Scanner exited with {:?}", output.status.code()),
        }
    } else {
        truncate_output(detail, 1024)
    };
    VirusResult { status, message }
}

/// Try to locate Windows Defender `MpCmdRun.exe` (newest Platform version preferred).
#[cfg(windows)]
pub fn find_defender() -> Option<PathBuf> {
    let platform_root =
        PathBuf::from(r"C:\ProgramData\Microsoft\Windows Defender\Platform");
    if let Ok(entries) = std::fs::read_dir(&platform_root) {
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("MpCmdRun.exe"))
            .filter(|p| p.exists())
            .collect();
        // Version directories sort lexicographically close to version order; pick newest.
        candidates.sort();
        if let Some(p) = candidates.pop() {
            return Some(p);
        }
    }
    for fallback in [
        r"C:\Program Files\Windows Defender\MpCmdRun.exe",
        r"C:\Program Files (x86)\Windows Defender\MpCmdRun.exe",
    ] {
        let p = PathBuf::from(fallback);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(not(windows))]
pub fn find_defender() -> Option<PathBuf> {
    None
}

/// Defender scan command template for a given `MpCmdRun.exe` path.
pub fn defender_template(mp_cmd: &Path) -> String {
    format!(
        "\"{}\" -Scan -ScanType 3 -File \"{{file}}\"",
        mp_cmd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_substitutes_file() {
        let (prog, args) = build_command(
            r#""C:\Program Files\Defender\MpCmdRun.exe" -Scan -ScanType 3 -File "{file}""#,
            Path::new(r"C:\Users\me\Downloads\file.zip"),
        )
        .unwrap();
        assert!(prog.contains("MpCmdRun"));
        assert!(args.iter().any(|a| a.contains("file.zip")));
    }

    #[test]
    fn build_command_appends_when_missing_placeholder() {
        let (prog, args) = build_command("scanner --fast", Path::new(r"C:\a b\c.bin")).unwrap();
        assert_eq!(prog, "scanner");
        assert_eq!(args.last().map(String::as_str), Some(r"C:\a b\c.bin"));
    }

    #[test]
    fn classify_exit_codes() {
        assert_eq!(classify_exit(Some(0), "", ""), VirusStatus::Clean);
        assert_eq!(classify_exit(Some(2), "", ""), VirusStatus::Threat);
        assert_eq!(classify_exit(Some(1), "", ""), VirusStatus::Error);
        assert_eq!(classify_exit(Some(1), "Threat found", ""), VirusStatus::Threat);
        assert_eq!(classify_exit(None, "", ""), VirusStatus::Error);
    }
}
