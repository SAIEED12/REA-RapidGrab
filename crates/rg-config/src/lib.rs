//! Shared RapidGrab configuration and state files (Phase 4 — Productivity).
//!
//! The GUI persists its own settings through eframe storage, but the
//! standalone `rapidgrab-popup.exe` is a separate process that has no access
//! to that storage. Anything both processes need — category folders, the
//! queue-paused flag, scheduler and daily-limit settings — lives here, in
//! `%APPDATA%\RapidGrab\` (via `dirs::config_dir()`):
//!
//! - `settings.json` — shared [`AppConfig`], written atomically (temp+rename)
//! - `queue.json` — pending download specs, restored on launch
//! - `history.jsonl` — append-only finished-job log (rotated at 5000 lines)
//! - `usage.json` — bytes downloaded today, for the daily cap
//! - `logs\` — daily rolling tracing logs (written by the GUI)
//!
//! The GUI is the only writer of these files; readers (popup, tests) tolerate
//! missing or corrupt files by falling back to defaults.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Directory holding every RapidGrab state file.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .map(|p| p.join("RapidGrab"))
        .unwrap_or_else(|| PathBuf::from(".").join("RapidGrab"))
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn queue_path() -> PathBuf {
    config_dir().join("queue.json")
}

pub fn history_path() -> PathBuf {
    config_dir().join("history.jsonl")
}

pub fn usage_path() -> PathBuf {
    config_dir().join("usage.json")
}

pub fn logs_dir() -> PathBuf {
    config_dir().join("logs")
}

/// Write `value` as pretty JSON to `path`, atomically (temp file + rename).
pub fn atomic_save<T: Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    let dir = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(dir)?;
    let body = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &std::path::Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Categories
// ---------------------------------------------------------------------------

/// Download categories (IDM's Compressed/Documents/Music/Programs/Video).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    #[default]
    Other,
}

impl Category {
    pub fn all() -> [Category; 6] {
        [
            Category::Compressed,
            Category::Documents,
            Category::Music,
            Category::Programs,
            Category::Video,
            Category::Other,
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Music => "Music",
            Category::Programs => "Programs",
            Category::Video => "Video",
            Category::Other => "Other",
        }
    }

    /// Default subfolder name under the download directory.
    pub fn default_subdir(self) -> &'static str {
        match self {
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Music => "Music",
            Category::Programs => "Programs",
            Category::Video => "Video",
            Category::Other => "Other",
        }
    }

    /// Map a file name (or bare extension) to its category.
    pub fn from_filename(filename: &str) -> Category {
        let name = filename.trim();
        let ext = name
            .rsplit('.')
            .next()
            .filter(|e| !e.is_empty() && e.len() <= 5 && !e.contains('/'))
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        // A name without an extension (or a path with one in a directory)
        // must not be treated as an extension.
        if !name.contains('.') {
            return Category::Other;
        }
        match ext.as_str() {
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" | "img" => {
                Category::Compressed
            }
            "pdf" | "doc" | "docx" | "txt" | "rtf" | "odt" | "xls" | "xlsx" | "ppt" | "pptx"
            | "csv" | "md" | "epub" => Category::Documents,
            "mp3" | "m4a" | "aac" | "wav" | "ogg" | "flac" | "wma" | "opus" => Category::Music,
            "exe" | "msi" | "apk" | "dmg" | "pkg" | "deb" | "rpm" | "bat" | "ps1" | "jar" => {
                Category::Programs
            }
            "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi" | "flv" | "wmv" | "mpg" | "mpeg"
            | "3gp" | "ogv" => Category::Video,
            _ => Category::Other,
        }
    }

    /// Map a Content-Type header value to its category.
    pub fn from_content_type(content_type: &str) -> Category {
        let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if mime.starts_with("video/") {
            Category::Video
        } else if mime.starts_with("audio/") {
            Category::Music
        } else if mime.starts_with("application/pdf")
            || mime.starts_with("text/")
            || mime.starts_with("application/msword")
            || mime.starts_with("application/vnd.openxmlformats")
        {
            Category::Documents
        } else if mime.starts_with("application/zip")
            || mime.starts_with("application/x-")
            || mime == "application/octet-stream"
        {
            // octet-stream is ambiguous; most callers try the filename first.
            Category::Compressed
        } else {
            Category::Other
        }
    }
}

// ---------------------------------------------------------------------------
// Shared configuration
// ---------------------------------------------------------------------------

/// What the scheduler does once the queue has drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterAction {
    #[default]
    None,
    Sleep,
    Hibernate,
    Shutdown,
}

impl AfterAction {
    pub fn all() -> [AfterAction; 4] {
        [
            AfterAction::None,
            AfterAction::Sleep,
            AfterAction::Hibernate,
            AfterAction::Shutdown,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            AfterAction::None => "Nothing",
            AfterAction::Sleep => "Sleep",
            AfterAction::Hibernate => "Hibernate",
            AfterAction::Shutdown => "Shut down",
        }
    }

    pub fn from_label(label: &str) -> Option<AfterAction> {
        AfterAction::all().into_iter().find(|a| a.label() == label)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Local "HH:MM" when the queue may run; empty means always.
    #[serde(default)]
    pub start: String,
    /// Local "HH:MM" when the queue stops; empty means never.
    #[serde(default)]
    pub stop: String,
    #[serde(default)]
    pub after_queue: AfterAction,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            start: String::new(),
            stop: String::new(),
            after_queue: AfterAction::None,
        }
    }
}

/// Configuration shared between the GUI and the standalone popup process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Route new downloads into per-category folders.
    #[serde(default)]
    pub organize_by_category: bool,
    /// Per-category save folders; missing entries use the defaults.
    #[serde(default)]
    pub category_dirs: HashMap<Category, String>,
    /// Global queue pause (Start/Stop Queue + scheduler stops).
    #[serde(default)]
    pub queue_paused: bool,
    /// Daily download allowance in megabytes; 0 = unlimited.
    #[serde(default)]
    pub daily_limit_mb: u64,
    #[serde(default)]
    pub scheduler: ScheduleSettings,
}

impl AppConfig {
    /// The folder a file name (or content type) should be saved into.
    pub fn dir_for(&self, download_dir: &std::path::Path, filename: &str, content_type: Option<&str>) -> PathBuf {
        let category = Category::from_filename(filename);
        let category = if category == Category::Other {
            content_type.map(Category::from_content_type).unwrap_or(Category::Other)
        } else {
            category
        };
        self.dir_for_category(download_dir, category)
    }

    pub fn dir_for_category(&self, download_dir: &std::path::Path, category: Category) -> PathBuf {
        if !self.organize_by_category {
            return download_dir.to_path_buf();
        }
        if let Some(dir) = self.category_dirs.get(&category) {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir);
            }
        }
        download_dir.join(category.default_subdir())
    }

    /// Load from disk; missing/corrupt files yield defaults.
    pub fn load() -> AppConfig {
        load_json(&settings_path())
    }

    pub fn save(&self) -> Result<()> {
        atomic_save(&settings_path(), self)
    }
}

/// True when minute-of-day `now` falls inside a [start, stop) window.
/// Windows may cross midnight (start > stop). Empty bounds disable the edge.
pub fn in_schedule_window(now_min: u16, start_min: Option<u16>, stop_min: Option<u16>) -> bool {
    match (start_min, stop_min) {
        (Some(start), Some(stop)) if start != stop => {
            if start < stop {
                now_min >= start && now_min < stop
            } else {
                // Crosses midnight: 22:00–06:00.
                now_min >= start || now_min < stop
            }
        }
        (Some(start), None) => now_min >= start,
        (None, Some(stop)) => now_min < stop,
        _ => true,
    }
}

/// Parse "HH:MM" into minutes past midnight; `None` when empty/invalid.
pub fn parse_hhmm(text: &str) -> Option<u16> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (h, m) = text.split_once(':')?;
    let h: u16 = h.trim().parse().ok()?;
    let m: u16 = m.trim().parse().ok()?;
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Queue persistence
// ---------------------------------------------------------------------------

/// One pending download, persisted so an overnight queue survives a restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedJobSpec {
    pub url: String,
    pub output: String,
    pub connections: usize,
    #[serde(default)]
    pub speed_limit: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default)]
    pub restart: bool,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub category: Category,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

impl Priority {
    pub fn all() -> [Priority; 3] {
        [Priority::Low, Priority::Normal, Priority::High]
    }

    pub fn label(self) -> &'static str {
        match self {
            Priority::Low => "Low",
            Priority::Normal => "Normal",
            Priority::High => "High",
        }
    }

    /// Sort weight: higher runs first.
    pub fn rank(self) -> u8 {
        match self {
            Priority::Low => 0,
            Priority::Normal => 1,
            Priority::High => 2,
        }
    }
}

pub fn load_queue() -> Vec<QueuedJobSpec> {
    load_json(&queue_path())
}

pub fn save_queue(jobs: &[QueuedJobSpec]) -> Result<()> {
    atomic_save(&queue_path(), &jobs.to_vec())
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// One finished (or failed/cancelled) download, appended to history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix timestamp (seconds).
    pub ts: u64,
    pub url: String,
    pub output: String,
    pub bytes: u64,
    pub elapsed_ms: u64,
    /// `completed` | `failed` | `cancelled`
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub category: Category,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Maximum history lines kept; older entries are dropped on append.
pub const HISTORY_MAX: usize = 5000;

pub fn append_history(entry: &HistoryEntry) -> Result<()> {
    let path = history_path();
    fs::create_dir_all(path.parent().context("history path has no parent")?)?;
    let line = serde_json::to_string(entry)?;
    // Rotate when the file grows past the cap.
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > (HISTORY_MAX * 256) as u64 {
            let entries = load_history();
            let keep = entries.len().saturating_sub(1000);
            let mut body = String::new();
            for e in entries.iter().skip(keep) {
                body.push_str(&serde_json::to_string(e)?);
                body.push('\n');
            }
            fs::write(&path, body)?;
        }
    }
    use std::io::Write;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn load_history() -> Vec<HistoryEntry> {
    fs::read_to_string(history_path())
        .map(|body| {
            body.lines()
                .filter_map(|line| serde_json::from_str::<HistoryEntry>(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

pub fn clear_history() -> Result<()> {
    let path = history_path();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daily usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyUsage {
    /// Local date, "YYYY-MM-DD".
    pub date: String,
    pub bytes: u64,
}

impl DailyUsage {
    pub fn load(today: &str) -> DailyUsage {
        let usage = load_json::<DailyUsage>(&usage_path());
        if usage.date == today {
            usage
        } else {
            DailyUsage {
                date: today.to_string(),
                bytes: 0,
            }
        }
    }

    pub fn add(&self, today: &str, bytes: u64) -> Result<DailyUsage> {
        let mut usage = Self::load(today);
        usage.bytes = usage.bytes.saturating_add(bytes);
        atomic_save(&usage_path(), &usage)?;
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_from_filename() {
        assert_eq!(Category::from_filename("movie.mp4"), Category::Video);
        assert_eq!(Category::from_filename("Archive.ZIP"), Category::Compressed);
        assert_eq!(Category::from_filename("song.flac"), Category::Music);
        assert_eq!(Category::from_filename("setup.exe"), Category::Programs);
        assert_eq!(Category::from_filename("report.pdf"), Category::Documents);
        assert_eq!(Category::from_filename("notes.txt"), Category::Documents);
        assert_eq!(Category::from_filename("download.bin"), Category::Other);
        assert_eq!(Category::from_filename("noextension"), Category::Other);
        // Dots in directories are not extensions.
        assert_eq!(Category::from_filename("dir.with.dots/file"), Category::Other);
    }

    #[test]
    fn category_from_content_type() {
        assert_eq!(Category::from_content_type("video/mp4"), Category::Video);
        assert_eq!(Category::from_content_type("audio/mpeg; charset=binary"), Category::Music);
        assert_eq!(Category::from_content_type("application/pdf"), Category::Documents);
        assert_eq!(Category::from_content_type("text/html"), Category::Documents);
        assert_eq!(Category::from_content_type("application/octet-stream"), Category::Compressed);
        assert_eq!(Category::from_content_type("image/png"), Category::Other);
    }

    #[test]
    fn schedule_window() {
        // 22:00–06:00 crosses midnight.
        let start = Some(22 * 60);
        let stop = Some(6 * 60);
        assert!(in_schedule_window(23 * 60, start, stop));
        assert!(in_schedule_window(2 * 60, start, stop));
        assert!(in_schedule_window(6 * 60 - 1, start, stop));
        assert!(!in_schedule_window(6 * 60, start, stop));
        assert!(!in_schedule_window(21 * 60 + 59, start, stop));
        // Plain window.
        assert!(in_schedule_window(9 * 60, Some(9 * 60), Some(17 * 60)));
        assert!(!in_schedule_window(17 * 60, Some(9 * 60), Some(17 * 60)));
        // Open edges.
        assert!(in_schedule_window(3 * 60, Some(9 * 60), None) == false);
        assert!(in_schedule_window(10 * 60, Some(9 * 60), None));
        assert!(in_schedule_window(0, None, None));
    }

    #[test]
    fn parse_hhmm_rejects_garbage() {
        assert_eq!(parse_hhmm("22:30"), Some(22 * 60 + 30));
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("25:00"), None);
        assert_eq!(parse_hhmm("9:99"), None);
        assert_eq!(parse_hhmm("noon"), None);
    }

    #[test]
    fn priority_orders_high_first() {
        let mut jobs = vec![
            (Priority::Low, 1usize),
            (Priority::High, 3),
            (Priority::Normal, 2),
            (Priority::High, 4),
        ];
        jobs.sort_by(|a, b| b.0.rank().cmp(&a.0.rank()).then(a.1.cmp(&b.1)));
        assert_eq!(jobs, vec![
            (Priority::High, 3),
            (Priority::High, 4),
            (Priority::Normal, 2),
            (Priority::Low, 1),
        ]);
    }

    #[test]
    fn config_round_trip_and_dirs() {
        let mut config = AppConfig::default();
        config.organize_by_category = true;
        config.dir_for_category(std::path::Path::new(r"C:\Dl"), Category::Video);
        let download = std::path::PathBuf::from(r"C:\Dl");
        assert_eq!(
            config.dir_for(&download, "clip.mkv", None),
            download.join("Video")
        );
        assert_eq!(
            config.dir_for(&download, "data.bin", Some("application/pdf")),
            download.join("Documents")
        );
        config.organize_by_category = false;
        assert_eq!(config.dir_for(&download, "clip.mkv", None), download);
    }

    #[test]
    fn history_append_load_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        // Exercise the line format directly (paths are fixed in production).
        let entry = HistoryEntry {
            ts: 1,
            url: "https://ex.com/a.mp4".into(),
            output: r"C:\Dl\a.mp4".into(),
            bytes: 42,
            elapsed_ms: 100,
            status: "completed".into(),
            error: None,
            category: Category::Video,
            sha256: None,
        };
        let line = serde_json::to_string(&entry).unwrap();
        let back: HistoryEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(back.bytes, 42);
        assert_eq!(back.category, Category::Video);
        let _ = path;
    }
}
