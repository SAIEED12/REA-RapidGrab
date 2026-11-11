use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use rg_config::{AfterAction, AppConfig, Category, Priority};
use rg_core::{Credentials, Download, NetworkConfig, Progress, ProxyConfig, ProxyMode};
use rg_grab::{GrabEvent, GrabFile, GrabRules, ScanOptions};
use rg_resolve::{MediaEvent, MediaProgress, MediaRequest, MediaStage, PairRequest, Tools};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::ui::{theme, widgets};

const SETTINGS_KEY: &str = "rapidgrab.settings";

/// Default number of downloads that may run at the same time.
const DEFAULT_MAX_CONCURRENT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Starting,
    Downloading,
    Resolving,
    Merging,
    Pausing,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Downloading | Self::Resolving | Self::Merging | Self::Pausing
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelIntent {
    Pause,
    Stop,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum FailureKind {
    #[default]
    Other,
    /// The remote file changed after a partial download began. Restarting
    /// discards the stale parts manifest and downloads from scratch.
    RemoteChanged,
    /// HTTP 401/407: the GUI opens a credentials dialog and retries.
    AuthRequired {
        status: u16,
        host: String,
        realm: Option<String>,
        proxy: bool,
    },
}

#[derive(Debug, Clone)]
pub struct DownloadUpdate {
    pub id: usize,
    /// Incremented on every (re)start; stale updates are ignored.
    pub run: u64,
    pub progress: Option<Progress>,
    pub status: JobStatus,
    pub error: Option<String>,
    /// Failure classification, set only when status is Failed.
    pub failure: FailureKind,
    /// Effective output path, when it differs from the requested one.
    pub output: Option<PathBuf>,
    /// Combined resolver progress, when the job fell back to yt-dlp.
    pub media: Option<MediaProgress>,
    /// Informational message for the job card (not a failure).
    pub notice: Option<String>,
}

/// A separate audio DASH rendition captured alongside the video track
/// (Facebook Reels). The GUI merges the two after the video download.
#[derive(Debug, Clone)]
pub struct AudioTrack {
    pub url: String,
    /// Cookies scoped to the audio host (fall back to the video cookies).
    pub cookies: Option<String>,
}

impl AudioTrack {
    fn from_capture(capture: &rg_bridge::CaptureRequest) -> Option<AudioTrack> {
        let url = capture.audio_url.as_deref()?.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return None;
        }
        Some(AudioTrack {
            url,
            cookies: capture
                .audio_cookies
                .clone()
                .or_else(|| capture.cookies.clone()),
        })
    }
}

pub struct DownloadJob {
    pub id: usize,
    pub run: u64,
    pub url: String,
    pub output: PathBuf,
    pub connections: usize,
    pub speed_limit: Option<u64>,
    pub sha256: Option<String>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub cookie: Option<String>,
    pub restart: bool,
    /// Proxy settings captured when the job was enqueued; changing the global
    /// setting later only affects new jobs.
    pub network: NetworkConfig,
    pub status: JobStatus,
    pub progress: Option<Progress>,
    pub media: Option<MediaProgress>,
    pub error: Option<String>,
    /// Failure classification, set only when status is Failed.
    pub failure: FailureKind,
    /// Informational message shown under the status (for example a skipped
    /// checksum); cleared when the job starts again.
    pub notice: Option<String>,
    /// Last non-zero transfer rate. Progress samples arrive every 500 ms and
    /// can legitimately be zero for one interval (segments blocked on a speed
    /// limiter), which made the speed and ETA flicker; cards show this value
    /// instead so active jobs keep a stable rate.
    pub last_speed: f64,
    pub started_at: Option<Instant>,
    /// Accumulated run time of finished/previous runs (pauses).
    pub elapsed: Duration,
    pub cancel: Arc<AtomicBool>,
    pub intent: Option<CancelIntent>,
    pub remove_after_stop: bool,
    pub handle: Option<JoinHandle<()>>,
    /// Queue priority (High runs first) and auto-bucket category.
    pub priority: Priority,
    pub category: Category,
    /// When set, the job downloads this separate audio track and merges it
    /// into the final video with ffmpeg.
    pub audio: Option<AudioTrack>,
    /// History entry already written for the current terminal state.
    pub history_recorded: bool,
    /// Phase 6: result of the post-download virus scan, if run.
    pub virus: Option<crate::virus::VirusResult>,
}

impl DownloadJob {
    fn total_elapsed(&self) -> Duration {
        self.elapsed + self.started_at.map(|t| t.elapsed()).unwrap_or_default()
    }
}

/// Immutable snapshot used to draw the list without borrowing `jobs` mutably.
struct JobView {
    id: usize,
    url: String,
    output: PathBuf,
    connections: usize,
    speed_limit: Option<u64>,
    status: JobStatus,
    progress: Option<Progress>,
    media: Option<MediaProgress>,
    error: Option<String>,
    failure: FailureKind,
    notice: Option<String>,
    /// Smoothed transfer rate for the detail line and the header total.
    speed_bps: f64,
    elapsed: Duration,
    priority: Priority,
    category: Category,
    virus: Option<crate::virus::VirusResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    max_concurrent: usize,
    connections: usize,
    limit_kbps: String,
    user_agent: String,
    last_dir: String,
    #[serde(default = "default_auto_resolve")]
    auto_resolve: bool,
    #[serde(default = "default_max_height")]
    max_height: u32,
    #[serde(default)]
    yt_dlp_path: String,
    #[serde(default)]
    ffmpeg_path: String,
    #[serde(default)]
    yt_dlp_args: String,
    /// Hosts (lowercase, no `www.`) whose captures skip the popup and
    /// start immediately — the "don't ask again for this site" list.
    #[serde(default)]
    auto_start_hosts: Vec<String>,
    /// Proxy selection shared by direct downloads, browser probes and media
    /// streams. Captured per job when the job is created.
    #[serde(default)]
    proxy: ProxyConfig,
    // --- Phase 4: productivity (mirrored to rg-config settings.json) ---
    /// Route new downloads into per-category folders.
    #[serde(default = "default_organize")]
    organize_by_category: bool,
    /// Per-category save folders; missing entries use the defaults.
    #[serde(default)]
    category_dirs: std::collections::HashMap<Category, String>,
    /// Global queue pause (Start/Stop Queue + scheduler stops).
    #[serde(default)]
    queue_paused: bool,
    /// Daily download allowance in megabytes; 0 = unlimited.
    #[serde(default)]
    daily_limit_mb: usize,
    /// Scheduler: run the queue only inside the window.
    #[serde(default)]
    schedule_enabled: bool,
    #[serde(default)]
    schedule_start: String,
    #[serde(default)]
    schedule_stop: String,
    #[serde(default)]
    after_queue: AfterAction,

    // --- Phase 6: tray, notifications, auto-start, virus check ---
    #[serde(default = "default_true")]
    tray_enabled: bool,
    #[serde(default = "default_true")]
    close_to_tray: bool,
    #[serde(default = "default_true")]
    notify_on_complete: bool,
    #[serde(default = "default_true")]
    notify_on_failure: bool,
    /// Run at Windows sign-in (HKCU Run).
    #[serde(default)]
    start_with_windows: bool,
    /// When started with Windows, launch minimized to tray.
    #[serde(default = "default_true")]
    start_minimized: bool,
    #[serde(default)]
    virus_check_enabled: bool,
    /// Command template with `{file}` placeholder.
    #[serde(default)]
    virus_check_command: String,
}

fn default_auto_resolve() -> bool {
    true
}

fn default_organize() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_max_height() -> u32 {
    rg_resolve::DEFAULT_MAX_HEIGHT
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            connections: rg_core::DEFAULT_CONNECTIONS,
            limit_kbps: String::new(),
            user_agent: String::new(),
            last_dir: dirs::download_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            auto_resolve: default_auto_resolve(),
            max_height: default_max_height(),
            yt_dlp_path: String::new(),
            ffmpeg_path: String::new(),
            yt_dlp_args: String::new(),
            auto_start_hosts: Vec::new(),
            proxy: ProxyConfig::default(),
            organize_by_category: default_organize(),
            category_dirs: std::collections::HashMap::new(),
            queue_paused: false,
            daily_limit_mb: 0,
            schedule_enabled: false,
            schedule_start: String::new(),
            schedule_stop: String::new(),
            after_queue: AfterAction::None,
            tray_enabled: true,
            close_to_tray: true,
            notify_on_complete: true,
            notify_on_failure: true,
            start_with_windows: false,
            start_minimized: true,
            virus_check_enabled: false,
            virus_check_command: String::new(),
        }
    }
}

#[derive(Debug)]
enum Action {
    Pause(usize),
    Stop(usize),
    StartNow(usize),
    Resume(usize),
    Retry(usize),
    Restart(usize),
    Remove(usize),
    CyclePriority(usize),
    ToggleQueue,
    ShowPage(Page),
    HistoryRetry(usize),
    HistoryOpen(usize),
    HistoryFolder(usize),
    VirusCheck(usize),
    ClearHistory,
    GrabStart,
    GrabCancel,
    GrabAdd,
}

/// Top-level page shown below the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Downloads,
    History,
    Grabber,
}

/// One file found by the Site Grabber, with its checkbox state.
struct GrabResult {
    file: GrabFile,
    selected: bool,
}

/// Site Grabber page state (persisted through [`rg_config::GrabberSettings`]).
struct GrabberState {
    url: String,
    same_domain: bool,
    max_depth: usize,
    max_pages: usize,
    /// Selected preset names ("Video", "Audio", …).
    presets: Vec<String>,
    /// Free-form extension list as typed (comma/space separated).
    custom_extensions: String,
    include: String,
    exclude: String,
    respect_robots: bool,
    scanning: bool,
    cancel: Arc<AtomicBool>,
    results: Vec<GrabResult>,
    status: String,
    pages: usize,
    errors: usize,
}

impl GrabberState {
    fn from_settings(settings: &rg_config::GrabberSettings) -> Self {
        Self {
            url: settings.url.clone(),
            same_domain: settings.same_domain,
            max_depth: settings.max_depth.min(5),
            max_pages: settings.max_pages.clamp(1, 5000),
            presets: settings.presets.clone(),
            custom_extensions: settings.custom_extensions.clone(),
            include: settings.include.clone(),
            exclude: settings.exclude.clone(),
            respect_robots: settings.respect_robots,
            scanning: false,
            cancel: Arc::new(AtomicBool::new(false)),
            results: Vec::new(),
            status: String::new(),
            pages: 0,
            errors: 0,
        }
    }

    fn to_settings(&self) -> rg_config::GrabberSettings {
        rg_config::GrabberSettings {
            url: self.url.clone(),
            same_domain: self.same_domain,
            max_depth: self.max_depth,
            max_pages: self.max_pages,
            presets: self.presets.clone(),
            custom_extensions: self.custom_extensions.clone(),
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            respect_robots: self.respect_robots,
        }
    }

    fn selected_count(&self) -> usize {
        self.results.iter().filter(|result| result.selected).count()
    }
}

pub struct RapidGrabApp {
    rt: Arc<Runtime>,
    update_rx: Receiver<DownloadUpdate>,
    update_tx: Sender<DownloadUpdate>,
    capture_rx: Receiver<rg_bridge::CaptureRequest>,
    probe_rx: Receiver<(String, Option<u64>, Option<String>, bool)>,
    probe_tx: Sender<(String, Option<u64>, Option<String>, bool)>,
    grab_rx: Receiver<GrabEvent>,
    grab_tx: Sender<GrabEvent>,
    virus_rx: Receiver<(usize, u64, crate::virus::VirusResult)>,
    virus_tx: Sender<(usize, u64, crate::virus::VirusResult)>,
    tray_rx: Receiver<crate::tray::TrayCommand>,
    tray: Option<crate::tray::TrayHandle>,
    /// Shared queue-paused flag observed by the tray menu for live state/label.
    tray_paused: Arc<AtomicBool>,
    /// Becomes true if the user explicitly exits via tray/menu (bypasses close-to-tray).
    exiting: bool,
    #[allow(dead_code)]
    pipe_guard: Option<rg_bridge::PipeGuard>,
    jobs: HashMap<usize, DownloadJob>,
    next_id: usize,

    settings: Settings,

    // Add form
    new_url: String,
    new_output: String,
    new_connections: usize,
    new_limit_kbps: String,
    new_restart: bool,
    new_sha256: String,
    new_user_agent: String,
    new_referer: String,
    new_cookie: String,
    /// Optional HTTP Basic credentials for the next direct download.
    new_auth_user: String,
    new_auth_pass: String,
    show_add_panel: bool,
    /// Set when a button should move keyboard focus back to the URL field.
    focus_url: bool,
    /// Cached result of the "output path already exists" probe.
    output_probe: Option<OutputProbe>,
    /// Set on the first frame so the page starts at the top without
    /// restoring the previous session's scroll offset.
    page_reset_done: bool,

    /// Browser capture awaiting popup confirmation (browser integration).
    pending_capture: Option<PendingCapture>,
    /// Credentials dialog shown after a 401/407 failure.
    auth_prompt: Option<AuthPrompt>,
    /// Shared snapshot for the deferred popup viewport ('static callback).
    popup_shared: Arc<std::sync::Mutex<PopupShared>>,

    /// Phase 4/5: current page and page-specific state.
    page: Page,
    grabber: GrabberState,
    history_cache: Vec<rg_config::HistoryEntry>,
    /// Set while the queue has work; after-queue actions fire once it drains.
    scheduler_armed: bool,
    /// Ephemeral header notice (daily cap, scheduler stop, …).
    last_notice: Option<String>,

    logo: egui::TextureHandle,
}

/// Popup action written by the deferred viewport, consumed in `logic()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupAction {
    Start,
    Later,
    Cancel,
    Browse,
}

/// State shared between the app (in `logic()`) and the deferred popup
/// viewport (whose callback must be `'static`, so it cannot borrow `self`).
#[derive(Debug, Clone)]
struct PopupShared {
    filename: String,
    url: String,
    size_label: String,
    type_label: String,
    source_label: String,
    /// Pretty host for header chips (referer page host wins over CDN host).
    host_label: String,
    /// File-kind code for the painted header badge (0 file, 1 video, 2 audio, 3 image).
    file_kind: u8,
    /// Whether the reported size may be one 206 range chunk.
    size_partial: bool,
    /// Save-to directory.
    folder: String,
    /// Editable file-name stem (extension is separate and locked).
    name_stem: String,
    /// Locked extension including the dot.
    name_ext: String,
    output: String,
    /// When the source URL was last copied (brief "Copied" feedback).
    copied_at: Option<Instant>,
    dont_ask_again: bool,
    action: Option<PopupAction>,
    /// A sibling audio track will be merged into this download.
    audio_present: bool,
    /// The capture looks like a video-only DASH rendition; no audio track
    /// could be identified.
    audio_missing: bool,
    /// Full size of the sibling audio track, when the CDN reported it.
    audio_bytes: Option<u64>,
}

impl PopupShared {
    fn blank() -> Self {
        Self {
            filename: String::new(),
            url: String::new(),
            size_label: String::new(),
            type_label: String::new(),
            source_label: String::new(),
            host_label: String::new(),
            file_kind: 0,
            size_partial: false,
            folder: String::new(),
            name_stem: String::new(),
            name_ext: String::new(),
            output: String::new(),
            copied_at: None,
            dont_ask_again: false,
            action: None,
            audio_present: false,
            audio_missing: false,
            audio_bytes: None,
        }
    }
}

/// Split a comma/whitespace separated list, dropping empty entries.
fn split_list(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// Pretty display host: strip scheme / www / port, lowercase.
/// Prefer the page (referer) host over a sniffed CDN host.
fn pretty_host(url: &str) -> String {
    let s = url.trim();
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split('/').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    host.trim_start_matches("www.").to_ascii_lowercase()
}

/// File-kind code for the popup header badge:
/// 0 file, 1 video, 2 audio, 3 image.
fn file_kind_for(filename: &str, content_type: Option<&str>) -> u8 {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.starts_with("video/") {
        return 1;
    }
    if ct.starts_with("audio/") {
        return 2;
    }
    if ct.starts_with("image/") {
        return 3;
    }
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi" | "flv" => 1,
        "mp3" | "m4a" | "wav" | "ogg" | "flac" => 2,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" => 3,
        _ => 0,
    }
}

/// Split a full name into its editable stem and locked extension.
fn split_name(name: &str) -> (String, String) {
    if let Some(dot) = name.rfind('.') {
        if dot > 0 && name.len() < dot + 8 {
            return (name[..dot].to_string(), name[dot..].to_string());
        }
    }
    (name.to_string(), String::new())
}

/// Strip Windows-forbidden characters from the editable stem.
fn sanitize_stem(stem: &str) -> String {
    rg_core::sanitize_file_stem(stem)
}

/// Keep head + tail visible for long paths, ellipsizing the middle.
fn ellipsize_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head = max / 2;
    let tail = max - head - 1;
    let head: String = chars.iter().take(head).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}\u{2026}{tail}")
}

/// Ellipsize a URL at the end (keeps the beginning visible).
fn ellipsize_end(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head: String = chars.iter().take(max.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

/// Cached filesystem probe for the Save-as path, refreshed at most every 500 ms.
struct OutputProbe {
    path: String,
    checked: Instant,
    exists: bool,
}

/// Credentials dialog state opened when a job fails with 401/407.
struct AuthPrompt {
    job_id: usize,
    host: String,
    realm: Option<String>,
    proxy: bool,
    username: String,
    password: String,
}

/// A browser capture waiting for the user to confirm in the popup.
struct PendingCapture {    capture: rg_bridge::CaptureRequest,
    /// Editable Save-as path shown in the popup.
    output: String,
    /// Best filename we have so far (header > URL > tab title).
    filename: String,
    /// Save-to directory (from `output`'s parent).
    folder: String,
    /// Editable file-name stem (no extension).
    name_stem: String,
    /// Locked extension including the dot.
    name_ext: String,
    size: Option<u64>,
    content_type: Option<String>,
    /// Set when the probe request is still in flight.
    probing: bool,
    /// True when `size` may be one 206 range chunk, not the whole file.
    size_partial: bool,
    dont_ask_again: bool,
}

impl RapidGrabApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::ui::apply_theme(&cc.egui_ctx);
        // A dark title bar keeps the native frame consistent with the app.
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
        let rt = Arc::new(Runtime::new().expect("Failed to create tokio runtime"));
        let (update_tx, update_rx) = unbounded();
        let (capture_tx, capture_rx) = unbounded::<rg_bridge::CaptureRequest>();
        let (probe_tx, probe_rx) = unbounded::<(String, Option<u64>, Option<String>, bool)>();
        let (grab_tx, grab_rx) = unbounded::<GrabEvent>();
        let (virus_tx, virus_rx) = unbounded::<(usize, u64, crate::virus::VirusResult)>();
        let (tray_tx, tray_rx) = unbounded::<crate::tray::TrayCommand>();
        let repaint_ctx = cc.egui_ctx.clone();
        let pipe_guard = if cfg!(windows) {
            rg_bridge::listen(move |req| {
                let _ = capture_tx.send(req);
                // Wake the GUI so the deferred popup viewport opens at once;
                // the main window itself is never raised (only the small
                // Download window floats over the browser).
                repaint_ctx.request_repaint();
            })
            .ok()
        } else {
            None
        };

        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        // Shared Phase-4 fields live in rg-config's settings.json (the
        // standalone popup reads them from there). When that file exists it
        // is the source of truth and overlays the eframe copy (migration).
        let shared = AppConfig::load();
        let shared_exists = rg_config::settings_path().is_file();
        let mut settings = Settings {
            max_concurrent: settings.max_concurrent.clamp(1, 8),
            connections: settings.connections.clamp(1, 16),
            organize_by_category: if shared_exists {
                shared.organize_by_category
            } else {
                settings.organize_by_category
            },
            category_dirs: if shared_exists {
                shared.category_dirs.clone()
            } else {
                settings.category_dirs.clone()
            },
            queue_paused: if shared_exists {
                shared.queue_paused
            } else {
                settings.queue_paused
            },
            daily_limit_mb: if shared_exists {
                shared.daily_limit_mb as usize
            } else {
                settings.daily_limit_mb
            },
            schedule_enabled: if shared_exists {
                shared.scheduler.enabled
            } else {
                settings.schedule_enabled
            },
            schedule_start: if shared_exists {
                shared.scheduler.start.clone()
            } else {
                settings.schedule_start.clone()
            },
            schedule_stop: if shared_exists {
                shared.scheduler.stop.clone()
            } else {
                settings.schedule_stop.clone()
            },
            after_queue: if shared_exists {
                shared.scheduler.after_queue
            } else {
                settings.after_queue
            },
            ..settings
        };

        // Reflect actual registry state so the UI toggle is never stale.
        #[cfg(windows)]
        {
            settings.start_with_windows = crate::autostart::is_enabled();
        }
        let tray_paused = Arc::new(AtomicBool::new(settings.queue_paused));
        let tray = if settings.tray_enabled {
            crate::tray::start(cc.egui_ctx.clone(), tray_tx, tray_paused.clone())
        } else {
            None
        };

        let mut app = Self {
            rt,
            update_rx,
            update_tx,
            capture_rx,
            probe_rx,
            probe_tx,
            grab_rx,
            grab_tx,
            virus_rx,
            virus_tx,
            tray_rx,
            tray,
            tray_paused,
            exiting: false,
            pipe_guard,
            jobs: HashMap::new(),
            next_id: 1,
            new_connections: settings.connections,
            new_limit_kbps: settings.limit_kbps.clone(),
            new_user_agent: settings.user_agent.clone(),
            settings,
            new_url: String::new(),
            new_output: String::new(),
            new_restart: false,
            new_sha256: String::new(),
            new_referer: String::new(),
            new_cookie: String::new(),
            new_auth_user: String::new(),
            new_auth_pass: String::new(),
            show_add_panel: true,
            focus_url: false,
            output_probe: None,
            page_reset_done: false,
            pending_capture: None,
            auth_prompt: None,
            popup_shared: Arc::new(std::sync::Mutex::new(PopupShared::blank())),
            page: Page::Downloads,
            grabber: GrabberState::from_settings(&shared.grabber),
            history_cache: Vec::new(),
            scheduler_armed: false,
            last_notice: None,
            logo: crate::icon::logo_texture(&cc.egui_ctx),
        };
        app.restore_queue();
        app
    }

    /// The shared Phase-4 configuration, rebuilt from the live settings.
    fn shared_config(&self) -> AppConfig {
        AppConfig {
            organize_by_category: self.settings.organize_by_category,
            category_dirs: self.settings.category_dirs.clone(),
            queue_paused: self.settings.queue_paused,
            daily_limit_mb: self.settings.daily_limit_mb as u64,
            scheduler: rg_config::ScheduleSettings {
                enabled: self.settings.schedule_enabled,
                start: self.settings.schedule_start.clone(),
                stop: self.settings.schedule_stop.clone(),
                after_queue: self.settings.after_queue,
            },
            grabber: self.grabber.to_settings(),
        }
    }

    /// Persist the shared configuration for the popup process.
    fn persist_shared(&self) {
        if let Err(error) = self.shared_config().save() {
            tracing::warn!("could not save shared config: {error}");
        }
    }

    /// Update the logical queue-paused state and keep the tray menu in sync.
    fn apply_queue_paused(&mut self, paused: bool) {
        self.settings.queue_paused = paused;
        self.tray_paused.store(paused, Ordering::Relaxed);
    }

    /// Crawl rules built from the Grabber form.
    fn grabber_rules(&self) -> GrabRules {
        GrabRules {
            same_domain: self.grabber.same_domain,
            max_depth: self.grabber.max_depth.min(5),
            max_pages: self.grabber.max_pages.clamp(1, 5000),
            extensions: rg_grab::extensions_from(
                &self.grabber.presets,
                &split_list(&self.grabber.custom_extensions),
            ),
            include: split_list(&self.grabber.include),
            exclude: split_list(&self.grabber.exclude),
            respect_robots: self.grabber.respect_robots,
            delay: Duration::from_millis(120),
        }
    }

    /// Starts a Site Grabber scan on the tokio runtime.
    fn start_grab(&mut self) {
        if self.grabber.scanning {
            return;
        }
        let url = self.grabber.url.trim().to_string();
        if url.is_empty() {
            self.grabber.status = "Enter a site URL to scan.".to_string();
            return;
        }
        let rules = self.grabber_rules();
        let user_agent = if self.new_user_agent.trim().is_empty() {
            rg_core::DEFAULT_USER_AGENT.to_string()
        } else {
            self.new_user_agent.trim().to_string()
        };
        let options = ScanOptions {
            network: NetworkConfig {
                proxy: self.settings.proxy.clone(),
                credentials: None,
            },
            user_agent: Some(user_agent),
            cookie: (!self.new_cookie.trim().is_empty())
                .then(|| self.new_cookie.trim().to_string()),
        };

        self.grabber.results.clear();
        self.grabber.pages = 0;
        self.grabber.errors = 0;
        self.grabber.scanning = true;
        self.grabber.cancel = Arc::new(AtomicBool::new(false));
        self.grabber.status = "Scanning…".to_string();
        self.persist_shared();

        let tx = self.grab_tx.clone();
        let cancel = self.grabber.cancel.clone();
        self.rt.spawn(async move {
            let mut emit = |event: GrabEvent| {
                let _ = tx.send(event);
            };
            let report = rg_grab::scan(&url, &rules, &options, cancel, &mut emit).await;
            tracing::info!(
                pages = report.pages,
                files = report.files,
                errors = report.errors,
                cancelled = report.cancelled,
                "site scan finished"
            );
        });
    }

    fn cancel_grab(&mut self) {
        if self.grabber.scanning {
            self.grabber.cancel.store(true, Ordering::Relaxed);
            self.grabber.status = "Stopping…".to_string();
        }
    }

    /// Enqueues every checked grab result, using the page URL as Referer.
    fn add_selected(&mut self) {
        let files: Vec<GrabFile> = self
            .grabber
            .results
            .iter()
            .filter(|result| result.selected)
            .map(|result| result.file.clone())
            .collect();
        if files.is_empty() {
            self.grabber.status = "Select at least one file first.".to_string();
            return;
        }
        let connections = self.new_connections.max(1);
        let cookie =
            (!self.new_cookie.trim().is_empty()).then(|| self.new_cookie.trim().to_string());
        let count = files.len();
        for file in files {
            let output = self
                .routed_dir(&file.filename, None)
                .join(&file.filename)
                .display()
                .to_string();
            self.enqueue(
                file.url,
                output,
                connections,
                None,
                None,
                None,
                Some(file.page_url),
                cookie.clone(),
                false,
                None,
                None,
            );
        }
        self.grabber.status = format!("Added {count} file(s) to the queue.");
        self.persist_queue();
    }

    /// Drains scanner events (called from both `logic` and `ui`).
    fn pump_tray_commands(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                crate::tray::TrayCommand::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                crate::tray::TrayCommand::ToggleQueue => {
                    let paused = !self.settings.queue_paused;
                    self.apply_queue_paused(paused);
                    self.last_notice = Some(if paused {
                        "Queue stopped".to_string()
                    } else {
                        "Queue started".to_string()
                    });
                    self.persist_shared();
                }
                crate::tray::TrayCommand::OpenDownloads => {
                    let dir = dirs::download_dir()
                        .or_else(|| {
                            self.settings
                                .last_dir
                                .is_empty()
                                .then_some(std::path::PathBuf::from("."))
                                .or_else(|| Some(std::path::PathBuf::from(&self.settings.last_dir)))
                        })
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    let _ = open::that(dir);
                }
                crate::tray::TrayCommand::Exit => {
                    self.exiting = true;
                    if let Some(tray) = &self.tray {
                        tray.shutdown();
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn pump_virus_events(&mut self) {
        while let Ok((id, run, result)) = self.virus_rx.try_recv() {
            if let Some(job) = self.jobs.get_mut(&id) {
                if job.run == run {
                    let is_threat = result.status == crate::virus::VirusStatus::Threat;
                    let threat_msg = result.message.clone();
                    let name = job
                        .output
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "download".to_string());
                    job.virus = Some(result);
                    if is_threat {
                        self.last_notice = Some(format!("Threat detected: {name}"));
                        if let Some(tray) = &self.tray {
                            let msg = truncate_url(&format!("{name}: {threat_msg}"), 220);
                            tray.notify(
                                "Threat detected",
                                &msg,
                                crate::tray::BalloonKind::Warning,
                            );
                        }
                    }
                }
            }
        }
    }

    fn maybe_notify_complete(&self, file_name: &str, bytes: u64) {
        if !self.settings.notify_on_complete {
            return;
        }
        // Suppress balloons while the window is focused/visible enough to see.
        // (We can't easily know focus here without ctx; keep simple: notify always
        // unless tray disabled. Better: check visibility via ctx in logic, but this
        // method has no ctx - acceptable tradeoff.)
        if let Some(tray) = &self.tray {
            let msg = if bytes > 0 {
                format!("{} ({})", file_name, human_bytes(bytes))
            } else {
                file_name.to_string()
            };
            tray.notify("Download complete", &msg, crate::tray::BalloonKind::Info);
        }
    }

    fn maybe_notify_failure(&self, path: &std::path::Path, error: &str) {
        if !self.settings.notify_on_failure {
            return;
        }
        if let Some(tray) = &self.tray {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "download".to_string());
            let msg = truncate_url(&format!("{name}: {error}"), 220);
            tray.notify("Download failed", &msg, crate::tray::BalloonKind::Warning);
        }
    }

    fn maybe_start_virus_scan(&self, id: usize, run: u64, output: &std::path::Path) {
        if !self.settings.virus_check_enabled {
            return;
        }
        let cmd = self.settings.virus_check_command.trim();
        if cmd.is_empty() {
            return;
        }
        let cmd = cmd.to_string();
        let path = output.to_path_buf();
        let tx = self.virus_tx.clone();
        std::thread::Builder::new()
            .name("rapidgrab-virus-scan".into())
            .spawn(move || {
                let result = crate::virus::run_scan(&cmd, &path);
                let _ = tx.send((id, run, result));
            })
            .ok();
    }

    fn start_virus_scan_for(&self, id: usize) {
        let Some(job) = self.jobs.get(&id) else {
            return;
        };
        if !matches!(job.status, JobStatus::Completed) {
            return;
        }
        let cmd = self.settings.virus_check_command.trim();
        if cmd.is_empty() {
            return;
        }
        let cmd = cmd.to_string();
        let path = job.output.clone();
        let run = job.run;
        let tx = self.virus_tx.clone();
        std::thread::Builder::new()
            .name("rapidgrab-virus-scan".into())
            .spawn(move || {
                let result = crate::virus::run_scan(&cmd, &path);
                let _ = tx.send((id, run, result));
            })
            .ok();
    }

    fn pump_grab_events(&mut self) {
        while let Ok(event) = self.grab_rx.try_recv() {
            match event {
                GrabEvent::Page { .. } => {
                    self.grabber.pages += 1;
                    self.grabber.status = format!(
                        "Scanning… {} pages, {} files",
                        self.grabber.pages,
                        self.grabber.results.len()
                    );
                }
                GrabEvent::Found(file) => {
                    self.grabber.results.push(GrabResult {
                        file,
                        selected: true,
                    });
                }
                GrabEvent::Failed { error, .. } => {
                    self.grabber.errors += 1;
                    tracing::warn!("site scan: {error}");
                }
                GrabEvent::Done(report) => {
                    self.grabber.scanning = false;
                    self.grabber.status = if report.cancelled {
                        format!(
                            "Cancelled — {} files from {} pages",
                            report.files, report.pages
                        )
                    } else {
                        format!("Done — {} files from {} pages", report.files, report.pages)
                    };
                }
            }
        }
    }

    /// Folder a new download should be saved into (category routing).
    fn routed_dir(&self, filename: &str, content_type: Option<&str>) -> PathBuf {
        let base = if self.settings.last_dir.is_empty() {
            dirs::download_dir().unwrap_or_else(|| PathBuf::from("."))
        } else {
            PathBuf::from(&self.settings.last_dir)
        };
        self.shared_config().dir_for(&base, filename, content_type)
    }

    /// Pending jobs (queued/paused) written to `queue.json`.
    fn persist_queue(&self) {
        let specs: Vec<rg_config::QueuedJobSpec> = self
            .jobs
            .values()
            .filter(|j| matches!(j.status, JobStatus::Queued | JobStatus::Paused))
            .map(|j| rg_config::QueuedJobSpec {
                url: j.url.clone(),
                output: j.output.display().to_string(),
                connections: j.connections,
                speed_limit: j.speed_limit,
                sha256: j.sha256.clone(),
                user_agent: j.user_agent.clone(),
                referer: j.referer.clone(),
                cookie: j.cookie.clone(),
                restart: j.restart,
                priority: j.priority,
                category: j.category,
                audio_url: j.audio.as_ref().map(|a| a.url.clone()),
                audio_cookies: j.audio.as_ref().and_then(|a| a.cookies.clone()),
            })
            .collect();
        if let Err(error) = rg_config::save_queue(&specs) {
            tracing::warn!("could not save queue: {error}");
        }
    }

    /// Restore pending jobs from `queue.json` as Queued (paused until the
    /// pump runs). A queue that does not start downloading on its own is
    /// safer than one that silently resumes after a crash.
    fn restore_queue(&mut self) {
        for spec in rg_config::load_queue() {
            let audio = spec.audio_url.clone().map(|url| AudioTrack {
                url,
                cookies: spec.audio_cookies.clone().or_else(|| spec.cookie.clone()),
            });
            self.enqueue_with_priority(
                spec.url,
                spec.output,
                spec.connections,
                spec.speed_limit,
                spec.sha256,
                spec.user_agent,
                spec.referer,
                spec.cookie,
                spec.restart,
                None,
                spec.priority,
                audio,
            );
        }
    }

    /// Today as a local "YYYY-MM-DD" string (daily-cap bucket).
    fn today_str() -> String {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    }

    /// Drains the capture/probe channels and builds or updates the pending
    /// capture. Safe to call from both `logic()` (hidden window) and `ui()`.
    fn handle_captures(&mut self) {
        // Browser captures: intercept = Download popup, click/link = pre-fill.
        while let Ok(capture) = self.capture_rx.try_recv() {
            // Suggest into the category folder (unless the popup already
            // picked an output, which is always respected below). The shared
            // resolver owns the Content-Disposition → title → filename → URL
            // → fallback chain, so the GUI matches the standalone popup.
            let default_name = rg_core::default_capture_name(&rg_core::NamingContext {
                title: capture.title.as_deref(),
                content_disposition: capture.content_disposition_filename.as_deref(),
                filename: capture.filename.as_deref(),
                url: &capture.url,
                referer: capture.referer.as_deref(),
                content_type: capture.content_type.as_deref(),
                media_capture: capture.source.as_deref() == Some("media"),
            });
            // Never suggest a name that already exists: `name (1).ext`.
            let display_dir = self.routed_dir(&default_name, capture.content_type.as_deref());
            let display_name = rg_core::unique_file_name(&display_dir, &default_name);
            let (pending_stem, pending_ext) = split_name(&display_name);
            let suggested = display_dir.join(&display_name).display().to_string();
            let pending_folder = std::path::Path::new(&suggested)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| suggested.clone());

            let host = capture_host(&capture.url);
            let trusted = capture.auto_start
                || host
                    .as_deref()
                    .is_some_and(|h| self.settings.auto_start_hosts.iter().any(|t| t == h));

            // Confirmed by the browser-side popup process (rapidgrab-popup):
            // enqueue directly with the output the user picked there.
            if capture.confirmed {
                if let Some(output) = capture.output.clone() {
                    if capture.dont_ask_again {
                        if let Some(host) = capture_host(&capture.url) {
                            if !self.settings.auto_start_hosts.contains(&host) {
                                self.settings.auto_start_hosts.push(host);
                            }
                        }
                    }
                    // Always pre-fill the form as a fallback (confirm-first backup).
                    self.new_url = capture.url.clone();
                    self.new_output = output.clone();
                    if let Some(referer) = &capture.referer {
                        self.new_referer = referer.clone();
                    }
                    if let Some(cookies) = &capture.cookies {
                        self.new_cookie = cookies.clone();
                    }
                    if let Some(ua) = &capture.user_agent {
                        self.new_user_agent = ua.clone();
                    }
                    let speed = self
                        .new_limit_kbps
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .filter(|&x| x > 0)
                        .map(|x| x * 1024);
                    self.enqueue(
                        capture.url.clone(),
                        output,
                        self.new_connections.max(1),
                        speed,
                        None,
                        capture.user_agent.clone(),
                        capture.referer.clone(),
                        capture.cookies.clone(),
                        false,
                        None,
                        AudioTrack::from_capture(&capture),
                    );
                }
                self.show_add_panel = true;
                continue;
            }

            // Always pre-fill the form as a fallback (confirm-first backup).
            self.new_url = capture.url.clone();
            self.new_output = suggested.clone();
            if let Some(referer) = &capture.referer {
                self.new_referer = referer.clone();
            }
            if let Some(cookies) = &capture.cookies {
                self.new_cookie = cookies.clone();
            }
            if let Some(ua) = &capture.user_agent {
                self.new_user_agent = ua.clone();
            }

            if trusted {
                // Auto-start: queue immediately without the popup.
                let speed = self
                    .new_limit_kbps
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|&x| x > 0)
                    .map(|x| x * 1024);
                self.enqueue(
                    capture.url.clone(),
                    suggested,
                    self.new_connections.max(1),
                    speed,
                    None,
                    capture.user_agent.clone(),
                    capture.referer.clone(),
                    capture.cookies.clone(),
                    false,
                    None,
                    AudioTrack::from_capture(&capture),
                );
            } else {
                // Always probe: the browser-reported Content-Length can be
                // a single 206 range chunk (e.g. Facebook CDN), which would
                // show a bogus small size. It stays as a fallback and is
                // only replaced when the probe returns a real size.
                let size = capture.content_length;
                let content_type = capture.content_type.clone();
                self.pending_capture = Some(PendingCapture {
                    capture: capture.clone(),
                    output: suggested,
                    filename: display_name,
                    folder: pending_folder,
                    name_stem: pending_stem,
                    name_ext: pending_ext,
                    size,
                    content_type,
                    probing: true,
                    size_partial: false,
                    dont_ask_again: false,
                });
                self.spawn_probe(capture);
            }
            self.show_add_panel = true;
            self.focus_url = true;
        }
        while let Ok((url, size, ctype, partial)) = self.probe_rx.try_recv() {
            if let Some(pending) = &mut self.pending_capture {
                if pending.capture.url == url {
                    // Keep the browser-reported values as fallback when the
                    // probe comes back without a size/type. The probe's
                    // `partial` verdict already accounts for the fallback.
                    if size.is_some() {
                        pending.size = size;
                    }
                    pending.size_partial = partial;
                    if pending.content_type.is_none() {
                        pending.content_type = ctype;
                    }
                    pending.probing = false;
                }
            }
        }
    }

    /// Suggests `<download dir>/<name from url>` when the output field is empty.
    fn suggest_output(&mut self) {
        if !self.new_output.trim().is_empty() || self.new_url.trim().is_empty() {
            return;
        }
        let name = self
            .new_url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("download.bin");
        let dir = self.routed_dir(name, None);
        self.new_output = dir.join(name).display().to_string();
    }

    /// Probes the capture URL for size/type so the popup can show it
    /// (IDM's "Download File Info" behaviour). Best-effort, never blocks.
    fn spawn_probe(&self, capture: rg_bridge::CaptureRequest) {
        let tx = self.probe_tx.clone();
        let rt = self.rt.clone();
        let network = NetworkConfig {
            proxy: self.settings.proxy.clone(),
            credentials: None,
        };
        rt.spawn(async move {
            let client = match network.client_for(&capture.url, 1).await {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!("size probe skipped: {error}");
                    let _ = tx.send((capture.url.clone(), None, None, false));
                    return;
                }
            };
            let mut req = client.head(&capture.url);
            if let Some(ua) = &capture.user_agent {
                req = req.header(reqwest::header::USER_AGENT, ua.as_str());
            }
            if let Some(cookie) = &capture.cookies {
                req = req.header(reqwest::header::COOKIE, cookie.as_str());
            }
            if let Some(referer) = &capture.referer {
                req = req.header(reqwest::header::REFERER, referer.as_str());
            }
            // Total from `Content-Range: bytes 0-0/123456` (after '/') —
            // correct on 206 responses where Content-Length is one chunk.
            let range_total = |headers: &reqwest::header::HeaderMap| -> Option<u64> {
                let s = headers
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())?;
                s.rfind('/')
                    .and_then(|slash| s[slash + 1..].trim().parse::<u64>().ok())
            };
            let header_u64 = |headers: &reqwest::header::HeaderMap| -> Option<u64> {
                headers
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
            };

            let mut got_total = false;
            let (mut size, mut ctype) =
                match tokio::time::timeout(Duration::from_secs(10), req.send()).await {
                    Ok(Ok(resp)) if resp.status().is_success() => {
                        let headers = resp.headers().clone();
                        let size = {
                            let total = range_total(&headers);
                            if total.is_some() {
                                got_total = true;
                            }
                            total.or_else(|| header_u64(&headers))
                        };
                        let ctype = headers
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        (size, ctype)
                    }
                    _ => (None, None),
                };

            // One-byte ranged GET: HEAD alone can still report a chunk size
            // on range-capable CDNs. Headers only — the body is never read,
            // so this doesn't download the file. The 206 Content-Range
            // total is authoritative when present.
            {
                let mut get = client
                    .get(&capture.url)
                    .header(reqwest::header::RANGE, "bytes=0-0");
                if let Some(ua) = &capture.user_agent {
                    get = get.header(reqwest::header::USER_AGENT, ua.as_str());
                }
                if let Some(cookie) = &capture.cookies {
                    get = get.header(reqwest::header::COOKIE, cookie.as_str());
                }
                if let Some(referer) = &capture.referer {
                    get = get.header(reqwest::header::REFERER, referer.as_str());
                }
                if let Ok(Ok(resp)) =
                    tokio::time::timeout(Duration::from_secs(10), get.send()).await
                {
                    if resp.status().is_success()
                        || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
                    {
                        let headers = resp.headers().clone();
                        if let Some(total) = range_total(&headers) {
                            size = Some(total);
                            got_total = true;
                        } else if size.is_none() {
                            size = header_u64(&headers);
                        }
                        if ctype.is_none() {
                            ctype = headers
                                .get(reqwest::header::CONTENT_TYPE)
                                .and_then(|v| v.to_str().ok())
                                .map(|s| s.to_string());
                        }
                    }
                }
            }
            // Partial when no total was ever reported and the file is a
            // video or suspiciously small — the number may be one chunk.
            let final_size = size.or(capture.content_length);
            let final_ctype = ctype.as_deref().or(capture.content_type.as_deref());
            let kind = file_kind_for(&capture.url, final_ctype);
            let partial = !got_total
                && final_size.is_some_and(|n| kind == 1 || n < 256 * 1024);
            let _ = tx.send((capture.url.clone(), size, ctype, partial));
        });
    }

    /// Starts the pending capture as a queued job from the popup fields.
    fn confirm_capture(&mut self, later: bool) {
        let Some(pending) = self.pending_capture.take() else {
            return;
        };
        let capture = pending.capture.clone();
        if pending.dont_ask_again {
            if let Some(host) = capture_host(&capture.url) {
                if !self.settings.auto_start_hosts.contains(&host) {
                    self.settings.auto_start_hosts.push(host);
                }
            }
        }
        let speed = self
            .new_limit_kbps
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&x| x > 0)
            .map(|x| x * 1024);
        // The popup edits folder + name stem (extension locked); recompose
        // the full save path here so enqueue sees the final value. A
        // direct output set outside the popup (Browse) is respected when
        // the name fields match it.
        let recomposed = std::path::Path::new(&pending.folder)
            .join(format!(
                "{}{}",
                sanitize_stem(&pending.name_stem),
                pending.name_ext
            ))
            .display()
            .to_string();
        let output = if recomposed.is_empty() {
            pending.output.clone()
        } else {
            recomposed
        };
        self.enqueue(
            capture.url.clone(),
            output,
            self.new_connections.max(1),
            speed,
            None,
            capture.user_agent.clone(),
            capture.referer.clone(),
            capture.cookies.clone(),
            false,
            None,
            AudioTrack::from_capture(&capture),
        );
        // `later` already queues (pump_queue starts when a slot is free, the
        // same as Start in this build; IDM starts a part immediately while
        // showing the dialog, we have no such distinction yet).
        let _ = later;
    }

    /// Whether Start would replace an existing file at the current output path.
    ///
    /// The probe is cached per path and refreshed at most every 500 ms. A
    /// leftover parts manifest means the run would resume instead of replace,
    /// so no warning is shown in that case.
    fn output_would_replace(&mut self) -> bool {
        let path = self.new_output.trim();
        if path.is_empty() {
            return false;
        }
        let now = Instant::now();
        if let Some(probe) = &self.output_probe {
            if probe.path == path && now.duration_since(probe.checked) < Duration::from_millis(500)
            {
                return probe.exists;
            }
        }
        let target = PathBuf::from(path);
        let resumable = rg_core::parts_dir_for(&target)
            .join("manifest.json")
            .is_file();
        let exists = target.is_file() && !resumable;
        self.output_probe = Some(OutputProbe {
            path: path.to_string(),
            checked: now,
            exists,
        });
        exists
    }

    fn add_download(&mut self) {
        if self.new_url.trim().is_empty() || self.new_output.trim().is_empty() {
            return;
        }

        let speed_limit = self
            .new_limit_kbps
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&x| x > 0)
            .map(|x| x * 1024);
        let sha256 = non_empty(&self.new_sha256);
        let user_agent = non_empty(&self.new_user_agent);
        let referer = non_empty(&self.new_referer);
        let cookie = non_empty(&self.new_cookie);
        let credentials = non_empty(&self.new_auth_user)
            .map(|username| Credentials::new(username, self.new_auth_pass.clone()));
        let url = self.new_url.trim().to_string();
        let output = self.new_output.trim().to_string();
        let connections = self.new_connections.max(1);
        let restart = self.new_restart;

        // Remember the form as the next default.
        self.settings.connections = self.new_connections;
        self.settings.limit_kbps = self.new_limit_kbps.clone();
        self.settings.user_agent = self.new_user_agent.clone();

        self.new_url.clear();
        self.new_sha256.clear();
        self.new_referer.clear();
        self.new_cookie.clear();
        self.new_auth_user.clear();
        self.new_auth_pass.clear();
        // The next download starts from a clean form: re-suggest the save path
        // from the next URL instead of silently reusing this one, and never
        // carry a one-shot "ignore existing parts" flag into another job.
        self.new_output.clear();
        self.new_restart = false;

        self.enqueue(
            url,
            output,
            connections,
            speed_limit,
            sha256,
            user_agent,
            referer,
            cookie,
            restart,
            credentials,
            None,
        );
    }

    /// Inserts a queued job from explicit values (popup / auto-start captures).
    /// Does not touch the New-download form.
    #[allow(clippy::too_many_arguments)]
    fn enqueue(
        &mut self,
        url: String,
        output: String,
        connections: usize,
        speed_limit: Option<u64>,
        sha256: Option<String>,
        user_agent: Option<String>,
        referer: Option<String>,
        cookie: Option<String>,
        restart: bool,
        credentials: Option<Credentials>,
        audio: Option<AudioTrack>,
    ) {
        self.enqueue_with_priority(
            url,
            output,
            connections,
            speed_limit,
            sha256,
            user_agent,
            referer,
            cookie,
            restart,
            credentials,
            Priority::Normal,
            audio,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_with_priority(
        &mut self,
        url: String,
        output: String,
        connections: usize,
        speed_limit: Option<u64>,
        sha256: Option<String>,
        user_agent: Option<String>,
        referer: Option<String>,
        cookie: Option<String>,
        restart: bool,
        credentials: Option<Credentials>,
        priority: Priority,
        audio: Option<AudioTrack>,
    ) {
        if url.trim().is_empty() || output.trim().is_empty() {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let network = NetworkConfig {
            proxy: self.settings.proxy.clone(),
            credentials: credentials.filter(|credentials| !credentials.is_empty()),
        };
        self.jobs.insert(
            id,
            DownloadJob {
                id,
                run: 0,
                url: url.trim().to_string(),
                output: PathBuf::from(output.trim()),
                connections: connections.max(1),
                speed_limit,
                sha256,
                user_agent,
                referer,
                cookie,
                restart,
                network,
                status: JobStatus::Queued,
                progress: None,
                media: None,
                error: None,
                failure: FailureKind::Other,
                notice: None,
                last_speed: 0.0,
                started_at: None,
                elapsed: Duration::ZERO,
                cancel: Arc::new(AtomicBool::new(false)),
                intent: None,
                remove_after_stop: false,
                handle: None,
                priority,
                category: Category::from_filename(
                    PathBuf::from(output.trim())
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .as_deref()
                        .unwrap_or(""),
                ),
                history_recorded: false,
                virus: None,
                audio,
            },
        );
        self.persist_queue();
    }

    /// Spawns the tokio task for a job. Caller guarantees the job is `Queued`.
    fn start_job(&mut self, id: usize) {
        let cancel = Arc::new(AtomicBool::new(false));
        let tx = self.update_tx.clone();

        let auto_resolve = self.settings.auto_resolve;
        let max_height = (self.settings.max_height > 0).then_some(self.settings.max_height);
        let yt_dlp_path = non_empty(&self.settings.yt_dlp_path).map(PathBuf::from);
        let ffmpeg_path = non_empty(&self.settings.ffmpeg_path).map(PathBuf::from);
        let yt_dlp_args = crate::args::split_args(&self.settings.yt_dlp_args);

        let Some(params) = self.jobs.get_mut(&id).map(|job| {
            job.run += 1;
            job.cancel = cancel.clone();
            job.status = JobStatus::Starting;
            job.error = None;
            job.failure = FailureKind::Other;
            job.notice = None;
            job.progress = None;
            job.media = None;
            job.intent = None;
            job.remove_after_stop = false;
            job.last_speed = 0.0;
            job.history_recorded = false;
            job.started_at = Some(Instant::now());
            // Consume the one-shot restart flag now that the restart has
            // begun, so the next retry resumes normally again.
            let restart = job.restart;
            job.restart = false;
            (
                job.run,
                job.url.clone(),
                job.output.clone(),
                job.connections,
                job.speed_limit,
                job.sha256.clone(),
                job.user_agent.clone(),
                job.referer.clone(),
                job.cookie.clone(),
                restart,
                job.network.clone(),
                job.audio.clone(),
            )
        }) else {
            return;
        };

        let (
            run,
            url,
            output,
            connections,
            speed_limit,
            sha256,
            user_agent,
            referer,
            cookie,
            restart,
            network,
            audio,
        ) = params;
        let task_cancel = cancel.clone();

        let handle = self.rt.spawn(async move {
            let send = |progress: Option<Progress>,
                        status: JobStatus,
                        error: Option<String>,
                        output: Option<PathBuf>,
                        media: Option<MediaProgress>| {
                let _ = tx.send(DownloadUpdate {
                    id,
                    run,
                    progress,
                    status,
                    error,
                    failure: FailureKind::Other,
                    output,
                    media,
                    notice: None,
                });
            };

            // Browser captures a DASH rendition pair (Facebook Reels): stream
            // both, then mux with ffmpeg. Falls back to the regular direct
            // download when the capture carries no separate audio URL.
            if let Some(audio) = audio {
                // Fail before downloading: a silent video is not an acceptable
                // silent surprise. The configured path wins over PATH.
                let ffmpeg = ffmpeg_path
                    .clone()
                    .or_else(|| rg_resolve::find_tool("ffmpeg"));
                let Some(ffmpeg) = ffmpeg else {
                    send(
                        None,
                        JobStatus::Failed,
                        Some(
                            "ffmpeg is required to merge this video's audio track; \
                             install it or set its path in Settings"
                                .to_string(),
                        ),
                        None,
                        None,
                    );
                    return;
                };

                let page_url = referer.clone().unwrap_or_else(|| url.clone());
                let mut request = PairRequest::new(
                    page_url.clone(),
                    url.clone(),
                    audio.url.clone(),
                    output.clone(),
                )
                .connections(connections)
                .restart(restart)
                .network(network.clone())
                .audio_cookie(
                    audio
                        .cookies
                        .clone()
                        .or_else(|| cookie.clone())
                        .unwrap_or_default(),
                );
                if let Some(limit) = speed_limit {
                    request = request.speed_limit(limit);
                }
                if let Some(ua) = &user_agent {
                    request = request.user_agent(ua.clone());
                }
                if let Some(r) = &referer {
                    request = request.referer(r.clone());
                }
                if let Some(c) = &cookie {
                    request = request.cookie(c.clone());
                }
                request = request.cancel_flag(task_cancel.clone());

                let tx_pair = tx.clone();
                let callback = move |event: MediaEvent| {
                    let (status, progress, output, media) = match event {
                        MediaEvent::Resolving => (JobStatus::Resolving, None, None, None),
                        MediaEvent::Resolved { output, .. } => {
                            (JobStatus::Merging, None, Some(output), None)
                        }
                        MediaEvent::Progress(progress) => {
                            let status = if progress.stage == MediaStage::Merging {
                                JobStatus::Merging
                            } else {
                                JobStatus::Downloading
                            };
                            (status, None, None, Some(progress))
                        }
                        MediaEvent::Merging => (JobStatus::Merging, None, None, None),
                        MediaEvent::Done { output, .. } => {
                            (JobStatus::Merging, None, Some(output), None)
                        }
                    };
                    let _ = tx_pair.send(DownloadUpdate {
                        id,
                        run,
                        progress,
                        status,
                        error: None,
                        failure: FailureKind::Other,
                        output,
                        media,
                        notice: None,
                    });
                };

                match rg_resolve::download_pair(&ffmpeg, request, callback).await {
                    Ok(path) => {
                        send(None, JobStatus::Completed, None, Some(path), None);
                    }
                    Err(error) if error.is_cancelled() => {
                        send(None, JobStatus::Cancelled, None, None, None);
                    }
                    Err(error) => {
                        // Distinguish "install ffmpeg" from "ffmpeg ran and
                        // the merge failed" — the latter usually means the
                        // captured companion held no audio track.
                        let hint = match &error {
                            rg_resolve::ResolveError::FfmpegMissing => {
                                "\nInstall ffmpeg or set its path in Settings."
                            }
                            _ => {
                                "\nThe separate audio track could not be merged. \
                                 The video is kept as a hidden .rgvideo sidecar \
                                 next to the output and is resumed on retry."
                            }
                        };
                        send(
                            None,
                            JobStatus::Failed,
                            Some(format!("{error}{hint}")),
                            None,
                            None,
                        );
                    }
                }
                return;
            }

            let mut dl = Download::new(url.clone(), output.clone())
                .connections(connections)
                .resume(!restart)
                .cancel_flag(task_cancel.clone())
                .network(network.clone());

            if let Some(limit) = speed_limit {
                dl = dl.speed_limit(limit);
            }
            if let Some(ref expected) = sha256 {
                dl = dl.sha256(expected.clone());
            }
            if let Some(ref ua) = user_agent {
                dl = dl.user_agent(ua.clone());
            }
            if let Some(ref r) = referer {
                dl = dl.referer(r.clone());
            }
            if let Some(ref c) = cookie {
                dl = dl.cookie(c.clone());
            }

            let tx2 = tx.clone();
            dl = dl.on_progress(move |p: Progress| {
                let output = p.output.clone();
                let _ = tx2.send(DownloadUpdate {
                    id,
                    run,
                    progress: Some(p),
                    status: JobStatus::Downloading,
                    error: None,
                    failure: FailureKind::Other,
                    output: Some(output),
                    media: None,
                    notice: None,
                });
            });

            match dl.run().await {
                Ok(stats) => {
                    send(None, JobStatus::Completed, None, Some(stats.output), None);
                }
                Err(rg_core::Error::Cancelled) => {
                    send(None, JobStatus::Cancelled, None, None, None);
                }
                Err(rg_core::Error::WebPage { .. }) if auto_resolve => {
                    if sha256.is_some() {
                        let _ = tx.send(DownloadUpdate {
                            id,
                            run,
                            progress: None,
                            status: JobStatus::Resolving,
                            error: None,
                            failure: FailureKind::Other,
                            output: None,
                            media: None,
                            notice: Some(
                                "SHA-256 is not verified for media resolved by yt-dlp; \
                                 restart with --allow-html or --no-resolve to check a direct file."
                                    .to_string(),
                            ),
                        });
                    }
                    send(None, JobStatus::Resolving, None, None, None);
                    let tools = match Tools::resolve_paths(yt_dlp_path, ffmpeg_path) {
                        Ok(tools) => tools,
                        Err(error) => {
                            send(None, JobStatus::Failed, Some(error.to_string()), None, None);
                            return;
                        }
                    };

                    let mut request = MediaRequest::new(url, output)
                        .connections(connections)
                        .restart(restart)
                        .network(network.clone())
                        .extra_args(yt_dlp_args);
                    if let Some(limit) = speed_limit {
                        request = request.speed_limit(limit);
                    }
                    if let Some(height) = max_height {
                        request = request.max_height(height);
                    }
                    request = request.cancel_flag(task_cancel);

                    let tx_media = tx.clone();
                    let callback = move |event: MediaEvent| {
                        let (status, progress, output, media) = match event {
                            MediaEvent::Resolving => (JobStatus::Resolving, None, None, None),
                            MediaEvent::Resolved { output, .. } => {
                                (JobStatus::Resolving, None, Some(output), None)
                            }
                            MediaEvent::Progress(progress) => {
                                let status = if progress.stage == MediaStage::Merging {
                                    JobStatus::Merging
                                } else {
                                    JobStatus::Downloading
                                };
                                (status, None, None, Some(progress))
                            }
                            MediaEvent::Merging => (JobStatus::Merging, None, None, None),
                            MediaEvent::Done { output, .. } => {
                                (JobStatus::Merging, None, Some(output), None)
                            }
                        };
                        let _ = tx_media.send(DownloadUpdate {
                            id,
                            run,
                            progress,
                            status,
                            error: None,
                            failure: FailureKind::Other,
                            output,
                            media,
                            notice: None,
                        });
                    };

                    match rg_resolve::download_media(&tools, request, callback).await {
                        Ok(path) => {
                            send(None, JobStatus::Completed, None, Some(path), None);
                        }
                        Err(error) if error.is_cancelled() => {
                            send(None, JobStatus::Cancelled, None, None, None);
                        }
                        Err(error) => {
                            send(None, JobStatus::Failed, Some(error.to_string()), None, None);
                        }
                    }
                }
                Err(rg_core::Error::AuthRequired {
                    status,
                    host,
                    realm,
                    proxy,
                }) => {
                    let what = if proxy { "Proxy" } else { "The server" };
                    let _ = tx.send(DownloadUpdate {
                        id,
                        run,
                        progress: None,
                        status: JobStatus::Failed,
                        error: Some(format!(
                            "{what} {host} requires a username and password (HTTP {status})"
                        )),
                        failure: FailureKind::AuthRequired {
                            status,
                            host,
                            realm,
                            proxy,
                        },
                        output: None,
                        media: None,
                        notice: None,
                    });
                }
                Err(error) if matches!(error, rg_core::Error::FtpAuth(_)) => {
                    let rg_core::Error::FtpAuth(detail) = error else {
                        unreachable!()
                    };
                    let host = reqwest::Url::parse(&url)
                        .ok()
                        .and_then(|parsed| parsed.host_str().map(str::to_string))
                        .unwrap_or_default();
                    let _ = tx.send(DownloadUpdate {
                        id,
                        run,
                        progress: None,
                        status: JobStatus::Failed,
                        error: Some(format!("The FTP server requires a login: {detail}")),
                        failure: FailureKind::AuthRequired {
                            status: 0,
                            host,
                            realm: None,
                            proxy: false,
                        },
                        output: None,
                        media: None,
                        notice: None,
                    });
                }
                Err(rg_core::Error::RemoteChanged) => {
                    let _ = tx.send(DownloadUpdate {
                        id,
                        run,
                        progress: None,
                        status: JobStatus::Failed,
                        error: Some(
                            "The remote file changed after the partial download began. \
                             Restart the download to discard the partial file and start over."
                                .to_string(),
                        ),
                        failure: FailureKind::RemoteChanged,
                        output: None,
                        media: None,
                        notice: None,
                    });
                }
                Err(e) => {
                    send(None, JobStatus::Failed, Some(e.to_string()), None, None);
                }
            }
        });

        if let Some(job) = self.jobs.get_mut(&id) {
            job.handle = Some(handle);
        }
    }

    fn pause_job(&mut self, id: usize) {
        if let Some(job) = self.jobs.get_mut(&id) {
            if job.status.is_active() {
                job.intent = Some(CancelIntent::Pause);
                job.status = JobStatus::Pausing;
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    fn stop_job(&mut self, id: usize) {
        if let Some(job) = self.jobs.get_mut(&id) {
            match job.status {
                JobStatus::Starting
                | JobStatus::Downloading
                | JobStatus::Resolving
                | JobStatus::Merging
                | JobStatus::Pausing => {
                    job.intent = Some(CancelIntent::Stop);
                    job.status = JobStatus::Pausing;
                    job.cancel.store(true, Ordering::Relaxed);
                }
                // Nothing is running: cancel immediately and keep any parts.
                JobStatus::Paused | JobStatus::Queued => {
                    job.intent = None;
                    job.status = JobStatus::Cancelled;
                    job.progress = None;
                    job.error = None;
                }
                _ => {}
            }
        }
    }

    /// Leave the queue and start this job even if the concurrency budget is
    /// already full (an explicit override of "Run at once").
    fn start_now(&mut self, id: usize) {
        if self
            .jobs
            .get(&id)
            .is_some_and(|job| job.status == JobStatus::Queued)
        {
            self.start_job(id);
        }
    }

    fn restart_job(&mut self, id: usize) {
        if let Some(job) = self.jobs.get_mut(&id) {
            if matches!(
                job.status,
                JobStatus::Paused | JobStatus::Cancelled | JobStatus::Failed
            ) {
                job.status = JobStatus::Queued;
                job.restart = true;
                job.error = None;
                job.failure = FailureKind::Other;
                job.intent = None;
            }
        }
    }

    fn resume_job(&mut self, id: usize) {
        if let Some(job) = self.jobs.get_mut(&id) {
            if matches!(
                job.status,
                JobStatus::Paused | JobStatus::Cancelled | JobStatus::Failed
            ) {
                job.status = JobStatus::Queued;
                job.error = None;
                job.intent = None;
            }
        }
    }

    fn remove_job(&mut self, id: usize) {
        let active = self
            .jobs
            .get(&id)
            .map(|j| j.status.is_active())
            .unwrap_or(false);
        if active {
            if let Some(job) = self.jobs.get_mut(&id) {
                job.intent = Some(CancelIntent::Stop);
                job.remove_after_stop = true;
                job.status = JobStatus::Pausing;
                job.cancel.store(true, Ordering::Relaxed);
            }
        } else {
            self.jobs.remove(&id);
        }
    }

    /// Starts queued jobs while the concurrency budget allows.
    fn pump_queue(&mut self) {
        if self.settings.queue_paused {
            return;
        }
        // Daily cap: once the allowance is used up, hold the queue until
        // tomorrow (the usage bucket resets on the local date).
        if self.settings.daily_limit_mb > 0 {
            let used = rg_config::DailyUsage::load(&Self::today_str()).bytes;
            if used >= (self.settings.daily_limit_mb as u64) * 1024 * 1024 {
                if self.last_notice.is_none() {
                    self.last_notice = Some(format!(
                        "Daily limit reached ({} used of {} MB) — queue resumes tomorrow",
                        human_bytes(used),
                        self.settings.daily_limit_mb
                    ));
                }
                return;
            }
        }
        let max = self.settings.max_concurrent.max(1);
        let active = self.jobs.values().filter(|j| j.status.is_active()).count();
        if active >= max {
            return;
        }

        let mut queued: Vec<(Priority, usize)> = self
            .jobs
            .iter()
            .filter(|(_, j)| j.status == JobStatus::Queued)
            .map(|(&id, j)| (j.priority, id))
            .collect();
        // High priority first, then oldest (lowest id) first.
        queued.sort_by(|a, b| b.0.rank().cmp(&a.0.rank()).then(a.1.cmp(&b.1)));

        for (_, id) in queued.into_iter().take(max - active) {
            self.start_job(id);
        }
    }

    fn apply_update(&mut self, update: DownloadUpdate) {
        let Some(job) = self.jobs.get_mut(&update.id) else {
            return;
        };
        if job.run != update.run {
            return; // stale update from a previous run
        }
        // Byte count for history/daily-cap bookkeeping, captured before the
        // terminal branches clear the progress.
        let bytes_done = job
            .progress
            .as_ref()
            .map(|p| p.downloaded)
            .or_else(|| job.media.as_ref().map(|m| m.downloaded))
            .unwrap_or(0);
        if let Some(output) = update.output.clone() {
            job.output = output;
        }
        if let Some(notice) = update.notice {
            job.notice = Some(notice);
        }

        // A 401/407 opens the credentials dialog after the job is marked failed.
        let auth_failure = match &update.failure {
            FailureKind::AuthRequired { .. } => Some(update.failure.clone()),
            _ => None,
        };

        match update.status {
            JobStatus::Starting
            | JobStatus::Downloading
            | JobStatus::Resolving
            | JobStatus::Merging => {
                if job.status != JobStatus::Pausing {
                    job.status = update.status;
                }
                let sample = update
                    .progress
                    .as_ref()
                    .map(|progress| progress.speed_bps)
                    .or_else(|| update.media.as_ref().map(|media| media.speed_bps))
                    .unwrap_or(0.0);
                if sample > 1.0 {
                    job.last_speed = sample;
                }
                if let Some(p) = update.progress {
                    job.progress = Some(p);
                }
                job.media = update.media;
            }
            JobStatus::Cancelled => {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                let intent = job.intent.take();
                // A pause is not a terminal state: no history entry.
                if intent != Some(CancelIntent::Pause) {
                    Self::record_history(job, "cancelled", bytes_done);
                }
                job.progress = None;
                job.media = None;
                job.status = match intent {
                    Some(CancelIntent::Pause) => JobStatus::Paused,
                    _ => JobStatus::Cancelled,
                };
                if job.remove_after_stop {
                    self.jobs.remove(&update.id);
                }
            }
            JobStatus::Completed => {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                // The last progress sample can be far smaller than the final
                // file (a fast single-stream download may complete before the
                // next 500 ms progress tick), so prefer the on-disk size,
                // which is authoritative once the download has finished.
                let bytes = std::fs::metadata(&job.output)
                    .map(|m| m.len())
                    .unwrap_or(bytes_done);
                Self::record_history(job, "completed", bytes);
                // Daily cap bookkeeping.
                if bytes > 0 {
                    if let Err(error) = rg_config::DailyUsage::default()
                        .add(&Self::today_str(), bytes)
                    {
                        tracing::warn!("could not record daily usage: {error}");
                    }
                }
                let output_path = job.output.clone();
                let file_name = output_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "download".to_string());
                job.progress = None;
                job.media = None;
                job.status = JobStatus::Completed;
                job.error = None;
                // Phase 6: notification + optional virus scan.
                self.maybe_notify_complete(&file_name, bytes);
                self.maybe_start_virus_scan(update.id, update.run, &output_path);
            }
            JobStatus::Failed => {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                Self::record_history(job, "failed", bytes_done);
                let err = update.error.clone().unwrap_or_else(|| "Download failed".to_string());
                let output_path = job.output.clone();
                job.progress = None;
                job.media = None;
                job.status = JobStatus::Failed;
                job.error = update.error;
                job.failure = update.failure;
                self.maybe_notify_failure(&output_path, &err);
            }
            _ => {}
        }

        if let Some(FailureKind::AuthRequired {
            host, realm, proxy, ..
        }) = auth_failure
        {
            self.auth_prompt = Some(AuthPrompt {
                job_id: update.id,
                host,
                realm,
                proxy,
                username: String::new(),
                password: String::new(),
            });
        }
    }

    /// Append one history entry for a terminal update (once per run).
    fn record_history(job: &mut DownloadJob, status: &str, bytes: u64) {
        if job.history_recorded {
            return;
        }
        job.history_recorded = true;
        let entry = rg_config::HistoryEntry {
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            url: job.url.clone(),
            output: job.output.display().to_string(),
            bytes,
            elapsed_ms: job.total_elapsed().as_millis() as u64,
            status: status.to_string(),
            error: job.error.clone(),
            category: job.category,
            sha256: job.sha256.clone(),
        };
        if let Err(error) = rg_config::append_history(&entry) {
            tracing::warn!("could not append history: {error}");
        }
    }

    fn handle_actions(&mut self, actions: Vec<Action>) {
        let mut queue_changed = false;
        for action in actions {
            match action {
                Action::Pause(id) => self.pause_job(id),
                Action::Stop(id) => self.stop_job(id),
                Action::StartNow(id) => self.start_now(id),
                Action::Resume(id) => self.resume_job(id),
                Action::Retry(id) => self.resume_job(id),
                Action::Restart(id) => self.restart_job(id),
                Action::Remove(id) => {
                    self.remove_job(id);
                    queue_changed = true;
                }
                Action::CyclePriority(id) => {
                    if let Some(job) = self.jobs.get_mut(&id) {
                        job.priority = match job.priority {
                            Priority::Low => Priority::Normal,
                            Priority::Normal => Priority::High,
                            Priority::High => Priority::Low,
                        };
                    }
                    queue_changed = true;
                }
                Action::ToggleQueue => {
                    self.apply_queue_paused(!self.settings.queue_paused);
                    self.persist_shared();
                }
                Action::ShowPage(page) => {
                    self.page = page;
                    if page == Page::History {
                        self.history_cache = rg_config::load_history();
                    }
                }
                Action::HistoryRetry(index) => {
                    if let Some(entry) = self.history_cache.get(index).cloned() {
                        self.enqueue(entry.url.clone(), entry.output.clone(), self.settings.connections.max(1), None, entry.sha256.clone(), None, None, None, false, None, None);
                        self.page = Page::Downloads;
                    }
                }
                Action::HistoryOpen(index) => {
                    if let Some(entry) = self.history_cache.get(index) {
                        Self::open_path(std::path::Path::new(&entry.output));
                    }
                }
                Action::HistoryFolder(index) => {
                    if let Some(entry) = self.history_cache.get(index) {
                        if let Some(parent) = std::path::Path::new(&entry.output).parent() {
                            Self::open_path(parent);
                        }
                    }
                }
                Action::ClearHistory => {
                    let _ = rg_config::clear_history();
                    self.history_cache.clear();
                }
                Action::GrabStart => self.start_grab(),
                Action::GrabCancel => self.cancel_grab(),
                Action::GrabAdd => self.add_selected(),
                Action::VirusCheck(id) => self.start_virus_scan_for(id),
            }
        }
        if queue_changed {
            self.persist_queue();
        }
    }

    /// One scheduler tick: apply the start/stop window, fire the after-queue
    /// action once the queue drains, and clear stale notices.
    fn scheduler_tick(&mut self) {
        if self.settings.schedule_enabled {
            let now = chrono::Local::now();
            let now_min = (now.format("%H").to_string().parse::<u16>().unwrap_or(0)) * 60
                + now.format("%M").to_string().parse::<u16>().unwrap_or(0);
            let inside = rg_config::in_schedule_window(
                now_min,
                rg_config::parse_hhmm(&self.settings.schedule_start),
                rg_config::parse_hhmm(&self.settings.schedule_stop),
            );
            if inside && self.settings.queue_paused {
                self.apply_queue_paused(false);
                self.persist_shared();
                self.last_notice = None;
            } else if !inside && !self.settings.queue_paused {
                self.apply_queue_paused(true);
                self.persist_shared();
                self.last_notice = Some("Scheduler: queue stopped outside the configured window".into());
            }
        }

        let active = self.jobs.values().filter(|j| j.status.is_active()).count();
        let queued = self.jobs.values().filter(|j| j.status == JobStatus::Queued).count();
        if active + queued > 0 {
            self.scheduler_armed = true;
        } else if self.scheduler_armed {
            self.scheduler_armed = false;
            match self.settings.after_queue {
                AfterAction::None => {}
                AfterAction::Sleep => Self::power_action("sleep"),
                AfterAction::Hibernate => Self::power_action("hibernate"),
                AfterAction::Shutdown => Self::power_action("shutdown"),
            }
        }
    }

    #[cfg(windows)]
    fn power_action(action: &str) {
        let result = match action {
            "sleep" => std::process::Command::new("rundll32.exe")
                .args(["powrprof.dll,SetSuspendState", "0,1,0"])
                .status()
                .map(|_| ()),
            "hibernate" => std::process::Command::new("shutdown.exe")
                .args(["/h"])
                .status()
                .map(|_| ()),
            "shutdown" => std::process::Command::new("shutdown.exe")
                .args(["/s", "/t", "60"])
                .status()
                .map(|_| ()),
            _ => Ok(()),
        };
        if let Err(error) = result {
            tracing::warn!("after-queue power action ({action}) failed: {error}");
        }
    }

    #[cfg(not(windows))]
    fn power_action(_action: &str) {}

    fn browse_output(&mut self) {
        let dir = if self.settings.last_dir.is_empty() {
            dirs::download_dir()
        } else {
            Some(PathBuf::from(&self.settings.last_dir))
        };
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.set_file_name("download.bin").save_file() {
            if let Some(parent) = path.parent() {
                self.settings.last_dir = parent.display().to_string();
            }
            self.new_output = path.display().to_string();
        }
    }

    fn open_path(path: &std::path::Path) {
        if let Err(e) = open::that(path) {
            tracing::warn!("could not open {}: {e}", path.display());
        }
    }

    fn job_views(&self) -> Vec<JobView> {
        let mut views: Vec<JobView> = self
            .jobs
            .values()
            .map(|j| JobView {
                id: j.id,
                url: j.url.clone(),
                output: j.output.clone(),
                connections: j.connections,
                speed_limit: j.speed_limit,
                status: j.status.clone(),
                progress: j.progress.clone(),
                media: j.media.clone(),
                error: j.error.clone(),
                failure: j.failure.clone(),
                notice: j.notice.clone(),
                speed_bps: if j.status.is_active() {
                    j.last_speed
                } else {
                    0.0
                },
                elapsed: j.total_elapsed(),
                priority: j.priority,
                category: j.category,
                virus: j.virus.clone(),
            })
            .collect();
        views.sort_by_key(|v| v.id);
        views
    }

    /// Opens the deferred Download viewport while a capture is pending.
    /// Called from `logic()` so it works even when the main window is
    /// minimized/hidden (immediate viewports require a visible parent pass).
    /// Credentials dialog for a 401/407 failure. Signing in stores the values
    /// on the job (and proxy settings for a 407) and retries it.
    fn show_auth_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.auth_prompt.as_ref() else {
            return;
        };
        let mut username = prompt.username.clone();
        let mut password = prompt.password.clone();
        let mut submit = false;
        let mut cancel = false;

        egui::Window::new("Authentication required")
            .id(egui::Id::unique("rg-auth-prompt"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(340.0);
                ui.spacing_mut().item_spacing.y = theme::SP_XS;
                let target = if prompt.proxy { "proxy" } else { "server" };
                ui.add(egui::Label::new(format!(
                    "{} is asking for a {target} username and password.",
                    prompt.host
                )));
                if let Some(realm) = &prompt.realm {
                    ui.add(egui::Label::new(theme::caption_tertiary(&format!(
                        "Realm: {realm}"
                    ))));
                }
                ui.add_space(theme::SP_XS);
                ui.add(egui::Label::new(theme::label("Username")));
                widgets::text_field(
                    ui,
                    widgets::Field::new("rg-auth-user", "username"),
                    &mut username,
                );
                ui.add(egui::Label::new(theme::label("Password")));
                widgets::text_field(
                    ui,
                    widgets::Field::new("rg-auth-pass", "password").password(true),
                    &mut password,
                );
                ui.add_space(theme::SP_S);
                ui.horizontal(|ui| {
                    if widgets::button(ui, "Cancel", widgets::ButtonKind::Secondary, true).clicked()
                    {
                        cancel = true;
                    }
                    if widgets::button(ui, "Sign in", widgets::ButtonKind::Primary, true).clicked()
                    {
                        submit = true;
                    }
                });
            });

        if submit {
            let (job_id, proxy) = (prompt.job_id, prompt.proxy);
            if let Some(job) = self.jobs.get_mut(&job_id) {
                if proxy {
                    job.network.proxy.username = username.clone();
                    job.network.proxy.password = password.clone();
                    // Future jobs should use the working proxy credentials too.
                    self.settings.proxy.username = username;
                    self.settings.proxy.password = password;
                } else {
                    job.network.credentials = Some(Credentials::new(username, password));
                }
                job.status = JobStatus::Queued;
                job.error = None;
                job.failure = FailureKind::Other;
                job.notice = None;
                job.started_at = None;
            }
            self.auth_prompt = None;
            self.start_job(job_id);
        } else if cancel {
            self.auth_prompt = None;
        }
    }

    /// Polls the update channel and forwards worker events to the job cards.
    fn show_capture_popup(&mut self, ctx: &egui::Context) {
        // Sync snapshot into the shared state the 'static callback renders.
        if let Some(pending) = &self.pending_capture {
            // A "media" capture is a page only when the URL carries no media
            // file name: MSE players (Facebook) resolve to the sniffed CDN
            // URL, which downloads directly just like any other file.
            let is_media_page = pending.capture.source.as_deref() == Some("media")
                && !rg_bridge::is_media_file_url(&pending.capture.url);
            let audio_bytes = pending.capture.audio_content_length;
            let audio_present = pending
                .capture
                .audio_url
                .as_deref()
                .map(|url| url.starts_with("http://") || url.starts_with("https://"))
                .unwrap_or(false);
            let audio_missing = !audio_present && pending.capture.audio_missing;
            let base_size = if pending.probing {
                None
            } else {
                pending.size
            };
            let (size_label, type_label) = if is_media_page {
                ("Video page".to_string(), String::new())
            } else {
                // While probing, the stored size may be the browser's
                // partial (206 range) value — don't show it until the
                // probe settles.
                let size_label = if pending.probing {
                    "Resolving\u{2026}".to_string()
                } else {
                    let combined = base_size.map(|video| video + audio_bytes.unwrap_or(0));
                    match combined {
                        Some(bytes) => human_bytes(bytes),
                        None => "Unknown size".to_string(),
                    }
                };
                let type_label = pending
                    .content_type
                    .as_deref()
                    .unwrap_or(if pending.probing {
                        "\u{2026}"
                    } else {
                        "\u{2014}"
                    })
                    .to_string();
                (size_label, type_label)
            };
            {
                let mut shared = self.popup_shared.lock().unwrap();
                shared.filename = pending.filename.clone();
                shared.url = pending.capture.url.clone();
                shared.size_label = size_label;
                shared.type_label = type_label;
                shared.source_label = pending
                    .capture
                    .source
                    .as_deref()
                    .unwrap_or("browser")
                    .to_string();
                shared.host_label = pending
                    .capture
                    .referer
                    .as_deref()
                    .map(pretty_host)
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| pretty_host(&pending.capture.url));
                shared.file_kind = file_kind_for(
                    &pending.filename,
                    pending.content_type.as_deref(),
                );
                shared.size_partial = pending.size_partial;
                shared.audio_present = audio_present;
                shared.audio_missing = audio_missing;
                shared.audio_bytes = audio_bytes;
                shared.folder = pending.folder.clone();
                shared.name_stem = pending.name_stem.clone();
                shared.name_ext = pending.name_ext.clone();
                shared.output = pending.output.clone();
                shared.dont_ask_again = pending.dont_ask_again;
            }
        }
        if self.pending_capture.is_none() {
            return;
        }

        let shared = self.popup_shared.clone();
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("rg-download-popup"),
            egui::ViewportBuilder::default()
                .with_title("New download \u{2014} RapidGrab")
                .with_inner_size([520.0, 304.0])
                .with_resizable(false)
                .with_active(true),
            move |ui, _class| {
                let mut state = shared.lock().unwrap();
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    state.action = Some(PopupAction::Cancel);
                }
                // Short type chip: "MP4" not "video/mp4".
                let type_chip = state
                    .type_label
                    .rsplit('/')
                    .next()
                    .unwrap_or(&state.type_label)
                    .to_ascii_uppercase();
                let container_folder = state.folder.clone();
                let ext_text = state.name_ext.clone();
                let full_name = format!("{}{}", sanitize_stem(&state.name_stem), state.name_ext);
                let full_url = state.url.clone();
                let kind = state.file_kind;
                let partial = state.size_partial;
                let audio_only = state.file_kind == 2 && state.source_label == "media";
                let audio_present = state.audio_present;
                let audio_missing = state.audio_missing;
                let size_label = state.size_label.clone();
                let host_label = state.host_label.clone();
                let copied_active = state
                    .copied_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(1500));
                if state.copied_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(1500)) {
                    state.copied_at = None;
                }

                // Footer pinned to the window bottom, no divider — spacing
                // only. Hints left, actions right (Cancel = ghost).
                egui::Panel::bottom("rg-popup-footer")
                    .frame(
                        egui::Frame::default()
                            .fill(theme::CARD)
                            .inner_margin(egui::Margin::symmetric(16, 8)),
                    )
                    .show(ui, |ui: &mut egui::Ui| {
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(theme::caption_tertiary(
                                "Enter to start \u{b7} Esc to cancel",
                            )));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = theme::SP_S;
                                // Fixed widths so the hint + buttons fit the
                                // 488px inner width without clipping.
                                if widgets::button_fixed_width(
                                    ui,
                                    "Start download",
                                    widgets::ButtonKind::Primary,
                                    true,
                                    124.0,
                                )
                                .clicked()
                                {
                                    state.action = Some(PopupAction::Start);
                                }
                                if widgets::button_fixed_width(
                                    ui,
                                    "Download Later",
                                    widgets::ButtonKind::Secondary,
                                    true,
                                    112.0,
                                )
                                .clicked()
                                {
                                    state.action = Some(PopupAction::Later);
                                }
                                if widgets::button_fixed_width(
                                    ui,
                                    "Cancel",
                                    widgets::ButtonKind::Ghost,
                                    true,
                                    64.0,
                                )
                                .clicked()
                                {
                                    state.action = Some(PopupAction::Cancel);
                                }
                            });
                        });
                    });

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::default()
                            .fill(theme::CARD)
                            .stroke(egui::Stroke::new(1.0, theme::STROKE_SOFT))
                            .corner_radius(egui::CornerRadius::same(12))
                            // Single 16px left edge, 8px top/bottom.
                            .inner_margin(egui::Margin::symmetric(16, 8)),
                    )
                    .show(ui, |ui: &mut egui::Ui| {
                        ui.spacing_mut().item_spacing.y = theme::SP_S;

                        // --- Header: painted badge + editable name + chips ---
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = theme::SP_M;
                            // 36x36 painted file-type badge (no font glyph).
                            let (rect, _resp) = ui.allocate_exact_size(
                                egui::vec2(36.0, 36.0),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter();
                            let r = egui::CornerRadius::same(10);
                            painter.rect_filled(rect, r, theme::CONTROL);
                            painter.rect_stroke(
                                rect,
                                r,
                                egui::Stroke::new(1.0, theme::STROKE_SOFT),
                                egui::StrokeKind::Middle,
                            );
                            let cx = rect.center().x;
                            let cy = rect.center().y;
                            match kind {
                                1 => {
                                    painter.add(egui::Shape::convex_polygon(
                                        vec![
                                            egui::pos2(cx - 6.0, cy - 8.0),
                                            egui::pos2(cx + 8.0, cy),
                                            egui::pos2(cx - 6.0, cy + 8.0),
                                        ],
                                        theme::ACCENT,
                                        egui::Stroke::NONE,
                                    ));
                                }
                                2 => {
                                    for (i, &h) in [6.0_f32, 12.0, 8.0].iter().enumerate() {
                                        let x = cx - 9.0 + i as f32 * 7.0;
                                        painter.rect_filled(
                                            egui::Rect::from_center_size(
                                                egui::pos2(x, cy),
                                                egui::vec2(3.0, h),
                                            ),
                                            egui::CornerRadius::same(1),
                                            theme::ACCENT,
                                        );
                                    }
                                }
                                3 => {
                                    painter.circle_filled(
                                        egui::pos2(cx - 4.0, cy - 5.0),
                                        3.0,
                                        theme::ACCENT,
                                    );
                                    painter.add(egui::Shape::line_segment(
                                        [egui::pos2(cx - 9.0, cy + 8.0), egui::pos2(cx - 2.0, cy - 1.0)],
                                        egui::Stroke::new(2.5, theme::ACCENT),
                                    ));
                                    painter.add(egui::Shape::line_segment(
                                        [egui::pos2(cx + 0.0, cy + 3.0), egui::pos2(cx + 9.0, cy + 8.0)],
                                        egui::Stroke::new(2.5, theme::ACCENT),
                                    ));
                                }
                                _ => {
                                    let doc = egui::Rect::from_center_size(
                                        egui::pos2(cx + 1.0, cy),
                                        egui::vec2(14.0, 18.0),
                                    );
                                    painter.rect_filled(doc, egui::CornerRadius::same(2), theme::ACCENT);
                                    painter.add(egui::Shape::convex_polygon(
                                        vec![
                                            egui::pos2(doc.right() - 5.0, doc.top()),
                                            egui::pos2(doc.right(), doc.top() + 5.0),
                                            egui::pos2(doc.right() - 5.0, doc.top() + 5.0),
                                        ],
                                        theme::TEXT,
                                        egui::Stroke::NONE,
                                    ));
                                }
                            }
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                // Inline-editable name stem; the dimmed
                                // extension sits DIRECTLY after it on the
                                // same line (zero gap, baseline-aligned).
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    let field_resp = widgets::text_field(
                                        ui,
                                        widgets::Field::new("rg-popup-name", "file name")
                                            .frameless()
                                            .title(),
                                        &mut state.name_stem,
                                    );
                                    if !field_resp.response.has_focus() {
                                        field_resp.response.on_hover_text(full_name.clone());
                                    }
                                    if !ext_text.is_empty() {
                                        ui.add_sized(
                                            egui::vec2(48.0, 28.0),
                                            egui::Label::new(
                                                egui::RichText::new(&ext_text)
                                                    .color(theme::TEXT_TERTIARY)
                                                    .size(theme::CAPTION_SIZE),
                                            ),
                                        );
                                    }
                                    if field_resp.submitted {
                                        state.action = Some(PopupAction::Start);
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    widgets::chip(ui, &size_label, theme::ACCENT);
                                    if !type_chip.is_empty()
                                        && type_chip != "\u{2014}"
                                        && type_chip != "\u{2026}"
                                    {
                                        widgets::chip(ui, &type_chip, theme::ACCENT);
                                    }
                                    if !host_label.is_empty() {
                                        widgets::chip(ui, &host_label, theme::ACCENT);
                                    }
                                    if partial {
                                        widgets::chip(
                                            ui,
                                            "Size may be incomplete",
                                            theme::WARNING,
                                        )
                                        .on_hover_text(
                                            "The server didn't report a full size; the final size may differ.",
                                        );
                                    }
                                    if audio_only {
                                        widgets::chip(
                                            ui,
                                            "Audio only",
                                            theme::WARNING,
                                        )
                                        .on_hover_text(
                                            "Only an audio track was found for this player. The file is not a video.",
                                        );
                                    }
                                    if audio_present {
                                        widgets::chip(
                                            ui,
                                            "Video + audio",
                                            theme::ACCENT,
                                        )
                                        .on_hover_text(
                                            "The player streamed the video and audio tracks separately; \
                                             they will be merged with ffmpeg automatically.",
                                        );
                                    } else if audio_missing {
                                        widgets::chip(
                                            ui,
                                            "Video only",
                                            theme::WARNING,
                                        )
                                        .on_hover_text(
                                            "No separate audio track was found for this player; \
                                             the file will not play any sound.",
                                        );
                                    }
                                });
                            });
                        });

                        // --- Save to: label above field (+ Change button) ----
                        ui.add(egui::Label::new(theme::caption("Save to")));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = theme::SP_S;
                            // Field + Change exactly fill the row — nothing
                            // can overflow or clip at any DPI.
                            let row_w = ui.available_rect_before_wrap().width();
                            let btn_w = 76.0;
                            let field_w = (row_w - btn_w - theme::SP_S).max(80.0);
                            // Subtle-background path box (middle-ellipsized,
                            // left-aligned).
                            let display_folder = ellipsize_middle(&container_folder, 52);
                            let full_folder = container_folder.clone();
                            let (rect, _resp) = ui.allocate_exact_size(
                                egui::vec2(field_w, 36.0),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter();
                            painter.rect_filled(rect, egui::CornerRadius::same(6), theme::CONTROL);
                            painter.rect_stroke(
                                rect,
                                egui::CornerRadius::same(6),
                                egui::Stroke::new(1.0, theme::STROKE_SOFT),
                                egui::StrokeKind::Middle,
                            );
                            let text_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 10.0, rect.top() + 2.0),
                                egui::pos2(rect.right() - 10.0, rect.bottom() - 2.0),
                            );
                            let lbl = ui.put(
                                text_rect,
                                egui::Label::new(theme::caption(&display_folder))
                                    
                                    .truncate(),
                            );
                            lbl.on_hover_text(full_folder);
                            if widgets::button_fixed_width(
                                ui,
                                "Change",
                                widgets::ButtonKind::Secondary,
                                true,
                                btn_w,
                            )
                            .clicked()
                            {
                                state.action = Some(PopupAction::Browse);
                            }
                        });

                        // --- Source: label above field (+ Copy button) -------
                        ui.add(egui::Label::new(theme::caption("Source")));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = theme::SP_S;
                            // SAME geometry as Save to: 76px tonal button +
                            // field filling the rest, identical edges.
                            let row_w = ui.available_rect_before_wrap().width();
                            let field_w = (row_w - 76.0 - theme::SP_S).max(80.0);
                            let display_url = ellipsize_end(&full_url, 64);
                            let (rect, _resp) = ui.allocate_exact_size(
                                egui::vec2(field_w, 36.0),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter();
                            painter.rect_filled(rect, egui::CornerRadius::same(6), theme::CONTROL);
                            painter.rect_stroke(
                                rect,
                                egui::CornerRadius::same(6),
                                egui::Stroke::new(1.0, theme::STROKE_SOFT),
                                egui::StrokeKind::Middle,
                            );
                            let text_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 10.0, rect.top() + 2.0),
                                egui::pos2(rect.right() - 10.0, rect.bottom() - 2.0),
                            );
                            let url_label = ui.put(
                                text_rect,
                                egui::Label::new(theme::caption_tertiary(&display_url))
                                    
                                    .truncate(),
                            );
                            url_label.on_hover_text(full_url.clone());
                            let copy_label = if copied_active { "Copied" } else { "Copy" };
                            if widgets::button_fixed_width(
                                ui,
                                copy_label,
                                widgets::ButtonKind::Secondary,
                                true,
                                76.0,
                            )
                            .clicked()
                            {
                                ui.ctx().copy_text(full_url.clone());
                                state.copied_at = Some(Instant::now());
                            }
                        });

                        ui.add_space(theme::SP_S);

                        widgets::checkbox(
                            ui,
                            "rg-popup-dontask",
                            &mut state.dont_ask_again,
                            "Don't ask again for this site",
                        );
                    });

                // Keyboard: Escape cancels (Enter is handled by the
                // Save-as field's `submitted` in the body panel).
                if state.action.is_none()
                    && ui.ctx().input(|i| i.key_pressed(egui::Key::Escape))
                {
                    state.action = Some(PopupAction::Cancel);
                }
                // Keep the popup repainting while it is up (probe updates).
                ui.ctx().request_repaint_after(Duration::from_millis(500));
            },
        );

        // Consume the viewport's edits/action from the shared state.
        let action = {
            let state = self.popup_shared.lock().unwrap();
            state.action
        };
        if let Some(pending) = &mut self.pending_capture {
            let state = self.popup_shared.lock().unwrap();
            pending.folder = state.folder.clone();
            pending.name_stem = state.name_stem.clone();
            pending.name_ext = state.name_ext.clone();
            pending.output = std::path::Path::new(&state.folder)
                .join(format!(
                    "{}{}",
                    sanitize_stem(&state.name_stem),
                    state.name_ext
                ))
                .display()
                .to_string();
            pending.dont_ask_again = state.dont_ask_again;
        }
        match action {
            Some(PopupAction::Cancel) => {
                self.pending_capture = None;
                self.popup_shared.lock().unwrap().action = None;
            }
            Some(PopupAction::Start) => {
                self.popup_shared.lock().unwrap().action = None;
                self.confirm_capture(false)
            }
            Some(PopupAction::Later) => {
                self.popup_shared.lock().unwrap().action = None;
                self.confirm_capture(true)
            }
            Some(PopupAction::Browse) => {
                self.popup_shared.lock().unwrap().action = None;
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    if let Some(pending) = &mut self.pending_capture {
                        pending.folder = dir.display().to_string();
                        pending.output = dir
                            .join(format!(
                                "{}{}",
                                sanitize_stem(&pending.name_stem),
                                pending.name_ext
                            ))
                            .display()
                            .to_string();
                    }
                }
            }
            None => {}
        }
    }
}

impl eframe::App for RapidGrabApp {
    /// Runs even when the main window is minimized/hidden (unlike `ui()`),
    /// so captures still open the popup while the GUI stays out of the way.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Close-to-tray: intercept the window close button.
        if ctx.input(|i| i.viewport().close_requested())
            && self.settings.tray_enabled
            && self.settings.close_to_tray
            && self.tray.is_some()
            && !self.exiting
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        while let Ok(update) = self.update_rx.try_recv() {
            self.apply_update(update);
        }
        self.pump_virus_events();
        self.pump_tray_commands(ctx);
        self.pump_grab_events();
        self.handle_captures();
        self.pump_queue();
        self.show_capture_popup(ctx);
        if self.pending_capture.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if self.grabber.scanning {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // Keep ticking while the window is minimized/hidden (close-to-tray).
        // `ui()` is not called then, so without this repaint request the
        // engine's progress/completion updates would sit unprocessed in the
        // channel until the window is shown again — the download would look
        // stuck and history/queue would not update.
        if self.jobs.values().any(|j| j.status.is_active()) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = Settings {
            max_concurrent: self.settings.max_concurrent,
            connections: self.new_connections,
            limit_kbps: self.new_limit_kbps.clone(),
            user_agent: self.new_user_agent.clone(),
            last_dir: self.settings.last_dir.clone(),
            auto_resolve: self.settings.auto_resolve,
            max_height: self.settings.max_height,
            yt_dlp_path: self.settings.yt_dlp_path.clone(),
            ffmpeg_path: self.settings.ffmpeg_path.clone(),
            yt_dlp_args: self.settings.yt_dlp_args.clone(),
            auto_start_hosts: self.settings.auto_start_hosts.clone(),
            proxy: self.settings.proxy.clone(),
            organize_by_category: self.settings.organize_by_category,
            category_dirs: self.settings.category_dirs.clone(),
            queue_paused: self.settings.queue_paused,
            daily_limit_mb: self.settings.daily_limit_mb,
            schedule_enabled: self.settings.schedule_enabled,
            schedule_start: self.settings.schedule_start.clone(),
            schedule_stop: self.settings.schedule_stop.clone(),
            after_queue: self.settings.after_queue,
            tray_enabled: self.settings.tray_enabled,
            close_to_tray: self.settings.close_to_tray,
            notify_on_complete: self.settings.notify_on_complete,
            notify_on_failure: self.settings.notify_on_failure,
            start_with_windows: self.settings.start_with_windows,
            start_minimized: self.settings.start_minimized,
            virus_check_enabled: self.settings.virus_check_enabled,
            virus_check_command: self.settings.virus_check_command.clone(),
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
        #[cfg(windows)]
        {
            let _ = crate::autostart::set_enabled(
                self.settings.start_with_windows,
                self.settings.start_minimized,
            );
        }
        // Mirror the shared fields for the standalone popup process.
        self.persist_shared();
        self.persist_queue();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(update) = self.update_rx.try_recv() {
            self.apply_update(update);
        }
        self.pump_grab_events();
        self.handle_captures();
        for job in self.jobs.values_mut() {
            if job.status.is_active() && job.handle.as_ref().is_some_and(|h| h.is_finished()) {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                job.status = JobStatus::Failed;
                job.error = Some("download worker exited unexpectedly".to_string());
            }
        }
        self.scheduler_tick();
        self.pump_queue();
        self.show_auth_prompt(&ctx);

        // Esc drops keyboard focus from the URL field (and any other input).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            ctx.memory_mut(|m| m.stop_text_input());
        }

        let mut actions: Vec<Action> = Vec::new();
        let views = self.job_views();
        let active = views.iter().filter(|v| v.status.is_active()).count();
        let queued = views
            .iter()
            .filter(|v| v.status == JobStatus::Queued)
            .count();
        let total_speed: f64 = views.iter().map(|v| v.speed_bps).sum();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 20,
                        right: 20,
                        top: 14,
                        bottom: 12,
                    }),
            )
            .show(ui, |ui| {
                let mut area = egui::ScrollArea::vertical()
                    .id_salt("rapidgrab-page")
                    .auto_shrink([false, false]);
                if !self.page_reset_done {
                    area = area.vertical_scroll_offset(0.0);
                    self.page_reset_done = true;
                }
                area.show(ui, |ui| {
                    widgets::content_column(ui, |ui| {
                        header(ui, self, active, queued, total_speed, &mut actions);
                        ui.add_space(theme::SP_M);

                        if let Some(notice) = self.last_notice.clone() {
                            widgets::warning_line(ui, &notice);
                            ui.add_space(theme::SP_S);
                        }
                        if self.settings.queue_paused {
                            widgets::info_line(ui, "Queue stopped — use Start Queue to resume.");
                            ui.add_space(theme::SP_S);
                        }

                        match self.page {
                            Page::History => {
                                history_section(ui, self, &mut actions);
                                return;
                            }
                            Page::Grabber => {
                                grabber_section(ui, self, &mut actions);
                                return;
                            }
                            Page::Downloads => {}
                        }

                        if self.show_add_panel {
                            add_download_card(ui, self);
                            ui.add_space(theme::SP_XL);
                        }

                        downloads_section(ui, self, &views, &mut actions);
                    });
                });
            });

        self.handle_actions(actions);
        self.pump_queue();

        // Repaint while anything is in flight so timers stay live.
        if self.jobs.values().any(|j| j.status == JobStatus::Pausing) {
            ctx.request_repaint();
        }
        if self.jobs.values().any(|j| j.status.is_active()) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Lowercase host without a leading `www.` for the auto-start list.
fn capture_host(url: &str) -> Option<String> {
    url.trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(['/', ':', '?', '#'])
        .next()
        .map(|h| {
            h.to_ascii_lowercase()
                .trim_start_matches("www.")
                .to_string()
        })
        .filter(|h| !h.is_empty())
}

fn media_stage_label(stage: MediaStage) -> &'static str {
    match stage {
        MediaStage::Video => "video",
        MediaStage::Audio => "audio",
        MediaStage::Merging => "merge",
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn truncate_url(url: &str, max: usize) -> String {
    if url.chars().count() <= max {
        url.to_string()
    } else {
        format!("{}…", url.chars().take(max - 1).collect::<String>())
    }
}

fn view_name(url: &str, output: &std::path::Path) -> String {
    if let Some(name) = output.file_name().and_then(|s| s.to_str()) {
        if !name.is_empty() && name != "download.bin" {
            return name.to_string();
        }
    }
    url.rsplit('/').next().unwrap_or(url).to_string()
}

/// App header: logo, title and the quiet global status/hide actions.
fn header(
    ui: &mut egui::Ui,
    app: &mut RapidGrabApp,
    active: usize,
    queued: usize,
    total_speed: f64,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.add(
            egui::Image::new(egui::load::SizedTexture::new(
                app.logo.id(),
                egui::vec2(32.0, 32.0),
            ))
            .corner_radius(theme::R_MEDIUM as u8),
        );
        ui.add_space(theme::SP_S);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add(egui::Label::new(theme::title("RapidGrab")));
            ui.add(egui::Label::new(theme::caption(
                "Fast, segmented downloads",
            )));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let queue_label = if app.settings.queue_paused {
                "Start Queue"
            } else {
                "Stop Queue"
            };
            if widgets::button(ui, queue_label, widgets::ButtonKind::Secondary, true).clicked() {
                actions.push(Action::ToggleQueue);
            }
            if widgets::button(ui, "Grabber", widgets::ButtonKind::Secondary, true).clicked() {
                actions.push(Action::ShowPage(Page::Grabber));
            }
            if widgets::button(ui, "History", widgets::ButtonKind::Secondary, true).clicked() {
                actions.push(Action::ShowPage(Page::History));
            }
            if widgets::button(ui, "Downloads", widgets::ButtonKind::Secondary, true).clicked() {
                actions.push(Action::ShowPage(Page::Downloads));
            }
            let label = if app.show_add_panel {
                "Hide"
            } else {
                "New download"
            };
            if widgets::button(ui, label, widgets::ButtonKind::Secondary, true).clicked() {
                app.show_add_panel = !app.show_add_panel;
                if app.show_add_panel {
                    app.focus_url = true;
                }
            }
            if active > 0 {
                widgets::chip(
                    ui,
                    &format!("{}/s", human_bytes(total_speed.max(0.0) as u64)),
                    theme::ACCENT,
                );
                let text = if queued > 0 {
                    format!("{active} active · {queued} queued")
                } else {
                    format!("{active} active")
                };
                ui.add(egui::Label::new(theme::caption(text)));
            }
        });
    });
}

/// Save-as row: field + Browse in one full-width row. The field width is
/// derived from the button's *painted* width, so the row's right edge is
/// exactly the card's inner right edge — the same edge as the URL field
/// above and the right-aligned "Start download" button below.
fn save_as_row(ui: &mut egui::Ui, app: &mut RapidGrabApp) {
    ui.add(egui::Label::new(theme::label("Save as")));

    // The button's real painted width (label + 2×SP_L), floored at BROWSE_W
    // so it never renders narrower than intended. Pass that exact width to
    // the button and derive the field width from it so the row spans
    // exactly the available width. `available_rect_before_wrap` (not
    // `available_width`) accounts for the horizontal wrapper's spacing.
    let button_w = widgets::button_width_for(ui, "Browse…").max(theme::BROWSE_W);
    let row_w = ui.available_rect_before_wrap().width();
    let field_w = (row_w - button_w - theme::SP_S).max(140.0);

    ui.allocate_ui_with_layout(
        egui::vec2(row_w, theme::FIELD_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = theme::SP_S;
            widgets::text_field(
                ui,
                widgets::Field::new("rg-output", "Choose where to save…")
                    .dim_hint()
                    .width(field_w),
                &mut app.new_output,
            );
            if widgets::button_fixed_width(
                ui,
                "Browse…",
                widgets::ButtonKind::Secondary,
                true,
                button_w,
            )
            .clicked()
            {
                app.browse_output();
            }
        },
    );
}

/// The Connections / Speed cap / Run at once settings, laid out as three
/// equal columns across the card width, stacked below ~480px.
fn settings_row(ui: &mut egui::Ui, app: &mut RapidGrabApp) {
    let header = |ui: &mut egui::Ui, text: &str| {
        ui.add(egui::Label::new(theme::label(text)));
    };
    if ui.available_width() >= 480.0 {
        ui.columns(3, |cols| {
            cols[0].vertical(|ui| {
                header(ui, "Connections");
                widgets::stepper(ui, "rg-connections", &mut app.new_connections, 1..=16);
            });
            cols[1].vertical(|ui| {
                header(ui, "Speed cap");
                widgets::text_field(
                    ui,
                    widgets::Field::new("rg-limit", "Unlimited")
                        .suffix("KB/s")
                        .width(theme::LIMIT_W),
                    &mut app.new_limit_kbps,
                );
            });
            cols[2].vertical(|ui| {
                header(ui, "Run at once");
                widgets::stepper(ui, "rg-concurrent", &mut app.settings.max_concurrent, 1..=8);
            });
        });
    } else {
        ui.vertical(|ui| {
            ui.add(egui::Label::new(theme::label("Connections")));
            widgets::stepper(ui, "rg-connections", &mut app.new_connections, 1..=16);
        });
        ui.add_space(theme::SP_S);
        ui.vertical(|ui| {
            ui.add(egui::Label::new(theme::label("Speed cap")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-limit", "Unlimited")
                    .suffix("KB/s")
                    .width(theme::LIMIT_W),
                &mut app.new_limit_kbps,
            );
        });
        ui.add_space(theme::SP_S);
        ui.vertical(|ui| {
            ui.add(egui::Label::new(theme::label("Run at once")));
            widgets::stepper(ui, "rg-concurrent", &mut app.settings.max_concurrent, 1..=8);
        });
    }
}

/// The "New download" form card.
fn add_download_card(ui: &mut egui::Ui, app: &mut RapidGrabApp) {
    let invalid = url_error(&app.new_url);
    let mut submitted = false;
    let mut start_clicked = false;

    widgets::card().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = theme::SP_XS;

        ui.add(egui::Label::new(theme::section("New download")));
        // The item spacing already contributes part of the gap; pad the rest
        // so the title sits 6 px above the first field label.
        ui.add_space((theme::TITLE_GAP - ui.spacing().item_spacing.y).max(0.0));

        ui.add(egui::Label::new(theme::label("URL")));
        if std::mem::take(&mut app.focus_url) {
            ui.memory_mut(|m| m.request_focus(ui.make_persistent_id("rg-url")));
        }
        let url = widgets::text_field(
            ui,
            widgets::Field::new("rg-url", "https://example.com/file.iso")
                .icon(theme::Icon::Link)
                .height(theme::FIELD_TALL_H)
                .invalid(invalid.is_some()),
            &mut app.new_url,
        );
        submitted = url.submitted;
        if url.response.lost_focus() {
            app.suggest_output();
        }
        if let Some(message) = invalid {
            ui.add(egui::Label::new(
                egui::RichText::new(message)
                    .font(egui::FontId::new(
                        theme::CAPTION_SIZE,
                        theme::proportional(),
                    ))
                    .color(theme::ERROR),
            ));
        }

        save_as_row(ui, app);
        if app.output_would_replace() {
            widgets::warning_line(
                ui,
                "A file with this name already exists and will be replaced.",
            );
        }

        ui.add_space(theme::SP_XS);
        settings_row(ui, app);

        widgets::divider(ui);

        widgets::collapsible(ui, "rg-advanced", "Advanced", false, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SP_XS;
            let sha = &mut app.new_sha256;
            let ua = &mut app.new_user_agent;
            form_columns(
                ui,
                |ui| {
                    ui.add(egui::Label::new(theme::label(
                        "SHA-256 checksum (optional)",
                    )));
                    widgets::text_field(ui, widgets::Field::new("rg-sha", "expected digest"), sha);
                },
                |ui| {
                    ui.add(egui::Label::new(theme::label("User-Agent (optional)")));
                    widgets::text_field(ui, widgets::Field::new("rg-ua", "Mozilla/5.0…"), ua);
                },
            );
            ui.add_space(theme::SP_XS);
            let auth_user = &mut app.new_auth_user;
            let auth_pass = &mut app.new_auth_pass;
            form_columns(
                ui,
                |ui| {
                    ui.add(egui::Label::new(theme::label("Username (optional)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-auth-site-user", "HTTP Basic auth"),
                        auth_user,
                    );
                },
                |ui| {
                    ui.add(egui::Label::new(theme::label("Password (optional)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-auth-site-pass", "password").password(true),
                        auth_pass,
                    );
                },
            );
            ui.add_space(theme::SP_XS);
            widgets::checkbox(
                ui,
                "rg-restart",
                &mut app.new_restart,
                "Restart — ignore existing parts",
            );
        });

        widgets::divider(ui);

        widgets::collapsible(ui, "rg-resolver", "Media resolver", false, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SP_XS;
            widgets::checkbox(
                ui,
                "rg-auto-resolve",
                &mut app.settings.auto_resolve,
                "Resolve video pages automatically with yt-dlp",
            );
            ui.add_space(theme::SP_XS);
            max_height_picker(ui, &mut app.settings.max_height);
            ui.add_space(theme::SP_XS);
            let yt_dlp = &mut app.settings.yt_dlp_path;
            let ffmpeg = &mut app.settings.ffmpeg_path;
            form_columns(
                ui,
                |ui| {
                    ui.add(egui::Label::new(theme::label("yt-dlp path")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-ytdlp", "auto-detected"),
                        yt_dlp,
                    );
                },
                |ui| {
                    ui.add(egui::Label::new(theme::label("ffmpeg path")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-ffmpeg", "auto-detected"),
                        ffmpeg,
                    );
                },
            );
            ui.add(egui::Label::new(theme::label("Extra yt-dlp args")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-ytargs", "--cookies \"C:\\Users\\me\\cookies.txt\""),
                &mut app.settings.yt_dlp_args,
            );
            ui.add(egui::Label::new(theme::caption_tertiary(
                "Leave paths blank to search PATH, pip, WinGet and Scoop.",
            )));
        });

        widgets::divider(ui);

        widgets::collapsible(ui, "rg-proxy", "Proxy", false, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SP_XS;
            proxy_fields(ui, &mut app.settings.proxy);
        });

        widgets::divider(ui);

        widgets::collapsible(ui, "rg-categories", "Categories", false, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SP_XS;
            widgets::checkbox(
                ui,
                "rg-organize",
                &mut app.settings.organize_by_category,
                "Save downloads into per-category folders",
            );
            ui.add_space(theme::SP_XS);
            for category in Category::all() {
                let current = app
                    .settings
                    .category_dirs
                    .get(&category)
                    .cloned()
                    .unwrap_or_else(|| {
                        dirs::download_dir()
                            .map(|dir| dir.join(category.default_subdir()).display().to_string())
                            .unwrap_or_default()
                    });
                let mut value = current;
                ui.add(egui::Label::new(theme::label(category.name())));
                widgets::text_field(
                    ui,
                    widgets::Field::new(
                        &format!("rg-category-{}", category.name().to_ascii_lowercase()),
                        "default folder",
                    ),
                    &mut value,
                );
                let default_dir = dirs::download_dir()
                    .map(|dir| dir.join(category.default_subdir()).display().to_string())
                    .unwrap_or_default();
                if value != default_dir {
                    app.settings.category_dirs.insert(category, value);
                } else {
                    app.settings.category_dirs.remove(&category);
                }
            }
            ui.add(egui::Label::new(theme::caption_tertiary(
                "Blank or default folders are recomputed from the download directory.",
            )));
        });

        widgets::divider(ui);

        widgets::collapsible(ui, "rg-scheduler", "Scheduler", false, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SP_XS;
            widgets::checkbox(
                ui,
                "rg-schedule-enabled",
                &mut app.settings.schedule_enabled,
                "Run the queue only inside the scheduled window (local time)",
            );
            ui.add_space(theme::SP_XS);
            form_columns(
                ui,
                |ui| {
                    ui.add(egui::Label::new(theme::label("Start (HH:MM, empty = always)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-schedule-start", "22:00"),
                        &mut app.settings.schedule_start,
                    );
                },
                |ui| {
                    ui.add(egui::Label::new(theme::label("Stop (HH:MM, empty = never)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-schedule-stop", "06:00"),
                        &mut app.settings.schedule_stop,
                    );
                },
            );
            ui.add_space(theme::SP_XS);
            let after_labels: Vec<&str> = AfterAction::all().iter().map(|a| a.label()).collect();
            let selected = AfterAction::all()
                .iter()
                .position(|a| *a == app.settings.after_queue)
                .unwrap_or(0);
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(theme::label("After the queue finishes")));
                if let Some(index) = widgets::dropdown(ui, "rg-after-queue", selected, &after_labels, theme::DROPDOWN_W) {
                    app.settings.after_queue = AfterAction::all()[index];
                }
            });
            ui.add_space(theme::SP_XS);
            widgets::stepper(ui, "rg-daily-limit", &mut app.settings.daily_limit_mb, 0..=1_000_000);
            ui.add(egui::Label::new(theme::caption_tertiary(
                "Daily download limit in MB (0 = unlimited). Usage resets at midnight.",
            )));
        });

        widgets::divider(ui);

        widgets::collapsible(ui, "rg-application", "Application", false, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SP_XS;
            widgets::checkbox(
                ui,
                "rg-tray-enabled",
                &mut app.settings.tray_enabled,
                "Show tray icon",
            );
            widgets::checkbox(
                ui,
                "rg-close-to-tray",
                &mut app.settings.close_to_tray,
                "Minimize to tray when the window is closed",
            );
            ui.add_space(theme::SP_XS);
            widgets::checkbox(
                ui,
                "rg-notify-complete",
                &mut app.settings.notify_on_complete,
                "Notify when a download completes",
            );
            widgets::checkbox(
                ui,
                "rg-notify-failure",
                &mut app.settings.notify_on_failure,
                "Notify when a download fails",
            );
            ui.add_space(theme::SP_XS);
            widgets::checkbox(
                ui,
                "rg-start-windows",
                &mut app.settings.start_with_windows,
                "Start RapidGrab with Windows",
            );
            widgets::checkbox(
                ui,
                "rg-start-minimized",
                &mut app.settings.start_minimized,
                "Start minimized to tray",
            );
            ui.add_space(theme::SP_XS);
            widgets::checkbox(
                ui,
                "rg-virus-enabled",
                &mut app.settings.virus_check_enabled,
                "Scan completed downloads with an external antivirus command",
            );
            ui.add(egui::Label::new(theme::label("Virus-check command ({file} placeholder)")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-virus-cmd", "scanner \"{file}\""),
                &mut app.settings.virus_check_command,
            );
            if widgets::button(
                ui,
                "Use Windows Defender",
                widgets::ButtonKind::Secondary,
                true,
            )
            .clicked()
            {
                if let Some(path) = crate::virus::find_defender() {
                    app.settings.virus_check_command = crate::virus::defender_template(&path);
                    app.settings.virus_check_enabled = true;
                } else {
                    app.last_notice = Some(
                        "Windows Defender (MpCmdRun.exe) was not found on this system.".to_string(),
                    );
                }
            }
            ui.add(egui::Label::new(theme::caption_tertiary(
                "Runs after a download completes. {file} is replaced with the downloaded file path; if omitted, the path is appended. Off by default.",
            )));
        });

        widgets::divider(ui);

        if app.settings.auto_resolve {
            widgets::info_line(
                ui,
                "Video pages are resolved with yt-dlp; direct file URLs skip it.",
            );
        }
        ui.add_space(theme::SP_XS);

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_start = url_error(&app.new_url).is_none() && !app.new_url.trim().is_empty();
                if widgets::button(
                    ui,
                    "Start download",
                    widgets::ButtonKind::Primary,
                    can_start,
                )
                .clicked()
                {
                    start_clicked = true;
                }
            });
        });
    });

    if submitted {
        app.suggest_output();
    }
    if (submitted || start_clicked)
        && url_error(&app.new_url).is_none()
        && !app.new_url.trim().is_empty()
    {
        // Derive a save path when the user has not chosen one yet.
        app.suggest_output();
        if !app.new_output.trim().is_empty() {
            app.add_download();
            app.focus_url = true;
        }
    }
}

/// Proxy mode plus the fields relevant to that mode.
fn proxy_fields(ui: &mut egui::Ui, proxy: &mut ProxyConfig) {
    const MODES: [(&str, ProxyMode); 3] = [
        ("Direct (no proxy)", ProxyMode::Off),
        ("System (WinHTTP/PAC)", ProxyMode::System),
        ("Manual", ProxyMode::Manual),
    ];
    let selected = MODES
        .iter()
        .position(|(_, mode)| *mode == proxy.mode)
        .unwrap_or(0);
    let labels: Vec<&str> = MODES.iter().map(|(label, _)| *label).collect();
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme::label("Mode")));
        if let Some(index) =
            widgets::dropdown(ui, "rg-proxy-mode", selected, &labels, theme::DROPDOWN_W)
        {
            proxy.mode = MODES[index].1;
        }
    });

    match proxy.mode {
        ProxyMode::Off => {
            ui.add(egui::Label::new(theme::caption_tertiary(
                "Connections go straight to the server, even when HTTP_PROXY is set.",
            )));
        }
        ProxyMode::System => {
            ui.add(egui::Label::new(theme::label("PAC URL (optional)")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-proxy-pac", "use the Windows auto-config URL"),
                &mut proxy.pac_url,
            );
        }
        ProxyMode::Manual => {
            ui.add(egui::Label::new(theme::label("Proxy address")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-proxy-url", "host:port, http://… or socks5://…"),
                &mut proxy.url,
            );
            ui.add_space(theme::SP_XS);
            let user = &mut proxy.username;
            let pass = &mut proxy.password;
            form_columns(
                ui,
                |ui| {
                    ui.add(egui::Label::new(theme::label("Username (optional)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-proxy-user", "proxy user"),
                        user,
                    );
                },
                |ui| {
                    ui.add(egui::Label::new(theme::label("Password (optional)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-proxy-pass", "proxy password").password(true),
                        pass,
                    );
                },
            );
        }
    }

    if proxy.mode != ProxyMode::Off {
        ui.add_space(theme::SP_XS);
        ui.add(egui::Label::new(theme::label("Bypass list (optional)")));
        widgets::text_field(
            ui,
            widgets::Field::new("rg-proxy-bypass", "localhost, 127.0.0.1, *.internal"),
            &mut proxy.no_proxy,
        );
    }
    ui.add(egui::Label::new(theme::caption_tertiary(
        "Stored in plain text in the app settings; proxy auth uses HTTP Basic.",
    )));
}

/// Two fields side by side when there is room; stacked below
/// [`theme::FORM_COLUMN_MIN`].
fn form_columns(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    if ui.available_width() >= theme::FORM_COLUMN_MIN {
        ui.columns(2, |columns| {
            left(&mut columns[0]);
            right(&mut columns[1]);
        });
    } else {
        left(ui);
        ui.add_space(theme::SP_S);
        right(ui);
    }
}

/// Max-height dropdown. Values outside the preset list stay selectable as a
/// custom entry so an existing config is never lost.
fn max_height_picker(ui: &mut egui::Ui, value: &mut u32) {
    const HEIGHTS: [(u32, &str); 6] = [
        (0, "Best available"),
        (2160, "2160p"),
        (1440, "1440p"),
        (1080, "1080p"),
        (720, "720p"),
        (480, "480p"),
    ];

    let mut values: Vec<u32> = HEIGHTS.iter().map(|(height, _)| *height).collect();
    let mut labels: Vec<String> = HEIGHTS.iter().map(|(_, label)| label.to_string()).collect();
    let selected = match values.iter().position(|height| height == value) {
        Some(index) => index,
        None => {
            values.push(*value);
            labels.push(format!("{value}p (custom)"));
            values.len() - 1
        }
    };
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();

    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme::label("Max height")));
        if let Some(index) =
            widgets::dropdown(ui, "rg-max-height", selected, &labels, theme::DROPDOWN_W)
        {
            *value = values[index];
        }
    });
}

/// Site Grabber page: crawl form plus the live list of found files.
fn grabber_section(ui: &mut egui::Ui, app: &mut RapidGrabApp, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme::section("Site Grabber")));
        widgets::chip(ui, &app.grabber.results.len().to_string(), theme::ACCENT);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let selected = app.grabber.selected_count();
            let add_enabled = !app.grabber.scanning && selected > 0;
            if widgets::button(
                ui,
                &format!("Add selected ({selected})"),
                widgets::ButtonKind::Primary,
                add_enabled,
            )
            .clicked()
            {
                actions.push(Action::GrabAdd);
            }
            if app.grabber.scanning {
                if widgets::button(ui, "Cancel scan", widgets::ButtonKind::Secondary, true)
                    .clicked()
                {
                    actions.push(Action::GrabCancel);
                }
            } else if widgets::button(ui, "Scan", widgets::ButtonKind::Primary, true).clicked() {
                actions.push(Action::GrabStart);
            }
        });
    });
    ui.add_space(theme::SP_S);

    widgets::card().show(ui, |ui| {
        ui.add(egui::Label::new(theme::label("Site URL")));
        let response = widgets::text_field(
            ui,
            widgets::Field::new("rg-grab-url", "https://example.com/downloads/"),
            &mut app.grabber.url,
        );
        if response.submitted {
            actions.push(Action::GrabStart);
        }

        ui.add_space(theme::SP_S);
        widgets::checkbox(
            ui,
            "rg-grab-same-domain",
            &mut app.grabber.same_domain,
            "Stay on this site (files on other hosts are still listed)",
        );
        widgets::checkbox(
            ui,
            "rg-grab-robots",
            &mut app.grabber.respect_robots,
            "Respect robots.txt",
        );

        ui.add_space(theme::SP_XS);
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(theme::label("Depth")));
            widgets::stepper(ui, "rg-grab-depth", &mut app.grabber.max_depth, 0..=5);
            ui.add_space(theme::SP_M);
            ui.add(egui::Label::new(theme::label("Max pages")));
            widgets::stepper(ui, "rg-grab-pages", &mut app.grabber.max_pages, 1..=5000);
        });

        ui.add_space(theme::SP_S);
        ui.add(egui::Label::new(theme::label("File types")));
        ui.spacing_mut().item_spacing.y = theme::SP_XS;
        widget_presets(ui, app);

        ui.add_space(theme::SP_XS);
        ui.add(egui::Label::new(theme::label("Extra extensions")));
        widgets::text_field(
            ui,
            widgets::Field::new("rg-grab-ext", "comma separated, e.g. mp4, mkv, pdf")
                .dim_hint(),
            &mut app.grabber.custom_extensions,
        );

        ui.add_space(theme::SP_S);
        widgets::collapsible(ui, "rg-grab-filters", "URL filters", false, |ui| {
            ui.add(egui::Label::new(theme::label("Include (substring or * wildcard)")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-grab-include", "*example.com/movies/*").dim_hint(),
                &mut app.grabber.include,
            );
            ui.add_space(theme::SP_XS);
            ui.add(egui::Label::new(theme::label("Exclude")));
            widgets::text_field(
                ui,
                widgets::Field::new("rg-grab-exclude", "*trailer*, *sample*").dim_hint(),
                &mut app.grabber.exclude,
            );
            ui.add(egui::Label::new(theme::caption_tertiary(
                "Filters apply to file URLs. Separate entries with commas or spaces.",
            )));
        });
    });

    if !app.grabber.status.is_empty() {
        ui.add_space(theme::SP_S);
        let color = if app.grabber.scanning {
            theme::ACCENT
        } else {
            theme::TEXT_SECONDARY
        };
        ui.add(egui::Label::new(egui::RichText::new(&app.grabber.status).color(color)));
    }

    ui.add_space(theme::SP_M);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme::section("Results")));
        widgets::chip(
            ui,
            &format!("{} selected", app.grabber.selected_count()),
            theme::ACCENT,
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::button(ui, "Clear", widgets::ButtonKind::Secondary, true).clicked() {
                app.grabber.results.clear();
            }
            if widgets::button(ui, "Select none", widgets::ButtonKind::Secondary, true).clicked()
            {
                for result in &mut app.grabber.results {
                    result.selected = false;
                }
            }
            if widgets::button(ui, "Select all", widgets::ButtonKind::Secondary, true).clicked() {
                for result in &mut app.grabber.results {
                    result.selected = true;
                }
            }
        });
    });
    ui.add_space(theme::SP_S);

    if app.grabber.results.is_empty() {
        widgets::card().show(ui, |ui| {
            widgets::empty_state(
                ui,
                theme::Icon::Link,
                "Nothing scanned yet",
                "Enter a page URL above; every matching file link will be listed here.",
            );
        });
        return;
    }

    for index in 0..app.grabber.results.len() {
        let file = app.grabber.results[index].file.clone();
        let label = format!(
            "{}  ·  {}",
            file.filename,
            file.extension.to_ascii_uppercase()
        );
        widgets::card().show(ui, |ui| {
            widgets::checkbox(
                ui,
                &format!("rg-grab-sel-{index}"),
                &mut app.grabber.results[index].selected,
                &label,
            );
            ui.add(
                egui::Label::new(theme::caption_tertiary(truncate_url(&file.url, 110)))
                    .truncate(),
            )
            .on_hover_text(&file.url);
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(theme::caption_tertiary(format!(
                        "found on {}",
                        view_host(&file.page_url)
                    )))
                    .truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, "Copy URL", widgets::ButtonKind::Secondary, true)
                        .clicked()
                    {
                        ui.ctx().copy_text(file.url.clone());
                    }
                });
            });
        });
        ui.add_space(theme::SP_S);
    }
}

/// Preset checkbox list; toggling keeps `grabber.presets` in sync.
fn widget_presets(ui: &mut egui::Ui, app: &mut RapidGrabApp) {
    for (name, _) in rg_grab::PRESETS {
        let name: &str = *name;
        let mut checked = app
            .grabber
            .presets
            .iter()
            .any(|preset| preset.eq_ignore_ascii_case(name));
        let id = format!("rg-grab-preset-{}", name.to_ascii_lowercase());
        if widgets::checkbox(ui, &id, &mut checked, name).changed() {
            if checked {
                if !app
                    .grabber
                    .presets
                    .iter()
                    .any(|preset| preset.eq_ignore_ascii_case(name))
                {
                    app.grabber.presets.push(name.to_string());
                }
            } else {
                app.grabber
                    .presets
                    .retain(|preset| !preset.eq_ignore_ascii_case(name));
            }
        }
    }
}

/// History page: finished downloads from `history.jsonl`, newest first.
fn history_section(ui: &mut egui::Ui, app: &mut RapidGrabApp, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme::section("History")));
        widgets::chip(ui, &app.history_cache.len().to_string(), theme::ACCENT);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::button(ui, "Clear history", widgets::ButtonKind::Secondary, true).clicked() {
                actions.push(Action::ClearHistory);
            }
        });
    });
    ui.add_space(theme::SP_S);

    if app.history_cache.is_empty() {
        widgets::card().show(ui, |ui| {
            widgets::empty_state(
                ui,
                theme::Icon::Download,
                "No history yet",
                "Finished downloads will appear here.",
            );
        });
        return;
    }

    let entries: Vec<rg_config::HistoryEntry> = app.history_cache.clone();
    for (index, entry) in entries.iter().enumerate().rev() {
        let name = entry
            .output
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&entry.output)
            .to_string();
        let status_color = match entry.status.as_str() {
            "completed" => theme::SUCCESS_MUTED,
            "failed" => theme::ERROR,
            _ => theme::NEUTRAL,
        };
        widgets::card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(theme::label(truncate_url(&name, 60))).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    widgets::chip(ui, &entry.status, status_color);
                    widgets::chip(ui, entry.category.name(), theme::TEXT_SECONDARY);
                });
            });
            ui.add(egui::Label::new(theme::caption_tertiary(view_host(&entry.url))).truncate());
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(theme::caption(format!(
                    "{} · {} · {}",
                    human_bytes(entry.bytes),
                    fmt_duration(Duration::from_millis(entry.elapsed_ms)),
                    format_history_time(entry.ts)
                ))));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, "Retry", widgets::ButtonKind::Secondary, true).clicked() {
                        actions.push(Action::HistoryRetry(index));
                    }
                    if widgets::button(ui, "Folder", widgets::ButtonKind::Secondary, true).clicked() {
                        actions.push(Action::HistoryFolder(index));
                    }
                    if widgets::button(ui, "Open", widgets::ButtonKind::Secondary, true).clicked() {
                        actions.push(Action::HistoryOpen(index));
                    }
                    if widgets::button(ui, "Copy URL", widgets::ButtonKind::Secondary, true).clicked() {
                        ui.ctx().copy_text(entry.url.clone());
                    }
                });
            });
            if let Some(error) = &entry.error {
                ui.add_space(theme::SP_XS);
                widgets::alert(ui, error, theme::ERROR);
            }
        });
        ui.add_space(theme::SP_M);
    }
}

fn format_history_time(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Downloads heading plus either the empty state or the scrolling card list.
fn downloads_section(
    ui: &mut egui::Ui,
    app: &mut RapidGrabApp,
    views: &[JobView],
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme::section("Downloads")));
        if !views.is_empty() {
            widgets::chip(ui, &views.len().to_string(), theme::ACCENT);
        }
        ui.add_space(theme::SP_S);
        widgets::chip(
            ui,
            &format!("max {} at once", app.settings.max_concurrent),
            theme::TEXT_SECONDARY,
        );
    });
    ui.add_space(theme::SP_S);

    if views.is_empty() {
        widgets::card().show(ui, |ui| {
            widgets::empty_state(
                ui,
                theme::Icon::Download,
                "No downloads yet",
                "Paste a file URL or a video page link to get started.",
            );
            ui.vertical_centered(|ui| {
                if widgets::button(ui, "New download", widgets::ButtonKind::Secondary, true)
                    .clicked()
                {
                    app.show_add_panel = true;
                    app.focus_url = true;
                }
                ui.add_space(theme::SP_XS);
            });
        });
        return;
    }

    let virus_scan_available = !app.settings.virus_check_command.trim().is_empty();
    for view in views {
        job_card(ui, view, actions, virus_scan_available);
        ui.add_space(theme::SP_M);
    }
}

struct ProgressView {
    fraction: Option<f32>,
    color: egui::Color32,
    detail: String,
}

fn progress_view(view: &JobView) -> ProgressView {
    if view.status == JobStatus::Completed {
        let size = std::fs::metadata(&view.output)
            .map(|m| m.len())
            .unwrap_or_default();
        return ProgressView {
            fraction: Some(1.0),
            color: theme::SUCCESS_MUTED,
            detail: format!(
                "{} · {} elapsed",
                human_bytes(size),
                fmt_duration(view.elapsed)
            ),
        };
    }

    if let Some(media) = &view.media {
        let fraction = media.overall.clamp(0.0, 1.0) as f32;
        let mut detail = format!("{:.0}%", fraction * 100.0);
        if media.total > 0 {
            detail.push_str(&format!(
                " · {} / {}",
                human_bytes(media.downloaded),
                human_bytes(media.total)
            ));
        }
        detail.push_str(&format!(" · {}", media_stage_label(media.stage)));
        if view.speed_bps > 1.0 {
            detail.push_str(&format!(" · {}/s", human_bytes(view.speed_bps as u64)));
        }
        return ProgressView {
            fraction: Some(fraction),
            color: theme::ACCENT,
            detail,
        };
    }

    if let Some(progress) = &view.progress {
        let fraction = if progress.total > 0 {
            (progress.downloaded as f64 / progress.total as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let mut detail = if progress.total > 0 {
            format!(
                "{:.0}% · {} / {}",
                fraction * 100.0,
                human_bytes(progress.downloaded),
                human_bytes(progress.total)
            )
        } else {
            format!("{} received", human_bytes(progress.downloaded))
        };
        if view.speed_bps > 1.0 {
            detail.push_str(&format!(" · {}/s", human_bytes(view.speed_bps as u64)));
        }
        if view.speed_bps > 1.0 && progress.total > progress.downloaded {
            let secs = (progress.total - progress.downloaded) as f64 / view.speed_bps;
            detail.push_str(&format!(
                " · ETA {}",
                fmt_duration(Duration::from_secs_f64(secs.max(0.0)))
            ));
        }
        if let Some(limit) = view.speed_limit {
            detail.push_str(&format!(" · cap {}/s", human_bytes(limit)));
        }
        return ProgressView {
            fraction: Some(fraction),
            color: theme::ACCENT,
            detail,
        };
    }

    let (fraction, color, detail) = match view.status {
        JobStatus::Queued => (
            Some(0.0),
            theme::NEUTRAL,
            format!("{} connections · waiting to start", view.connections),
        ),
        JobStatus::Paused => (
            Some(0.0),
            theme::NEUTRAL,
            "Paused — resume to continue".to_string(),
        ),
        JobStatus::Failed => (None, theme::ERROR, "Failed".to_string()),
        JobStatus::Cancelled => (None, theme::NEUTRAL, "Stopped".to_string()),
        _ => (None, theme::NEUTRAL, String::new()),
    };
    ProgressView {
        fraction,
        color,
        detail,
    }
}

fn open_folder(view: &JobView) {
    let dir = view
        .output
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    RapidGrabApp::open_path(&dir);
}

/// Number of icon actions a job in `status` shows, used to reserve exactly
/// the space the action row needs (and no more) for the detail text.
fn action_count(status: &JobStatus) -> usize {
    match status {
        JobStatus::Pausing => 0,
        JobStatus::Queued
        | JobStatus::Starting
        | JobStatus::Downloading
        | JobStatus::Resolving
        | JobStatus::Merging => 2,
        _ => 3,
    }
}

/// Icon-only actions for the current job state, with tooltips.
fn job_actions(ui: &mut egui::Ui, view: &JobView, actions: &mut Vec<Action>) {
    ui.spacing_mut().item_spacing.x = theme::SP_XS;
    match view.status {
        JobStatus::Queued => {
            if widgets::icon_button(ui, theme::Icon::Remove, "Remove from list", true).clicked() {
                actions.push(Action::Remove(view.id));
            }
            if widgets::icon_button(ui, theme::Icon::Play, "Start now", true).clicked() {
                actions.push(Action::StartNow(view.id));
            }
        }
        JobStatus::Starting
        | JobStatus::Downloading
        | JobStatus::Resolving
        | JobStatus::Merging => {
            if widgets::icon_button(ui, theme::Icon::Stop, "Cancel download", true).clicked() {
                actions.push(Action::Stop(view.id));
            }
            if widgets::icon_button(ui, theme::Icon::Pause, "Pause", true).clicked() {
                actions.push(Action::Pause(view.id));
            }
        }
        JobStatus::Pausing => {}
        JobStatus::Paused => {
            if widgets::icon_button(ui, theme::Icon::Remove, "Remove from list", true).clicked() {
                actions.push(Action::Remove(view.id));
            }
            if widgets::icon_button(ui, theme::Icon::Folder, "Open folder", true).clicked() {
                open_folder(view);
            }
            if widgets::icon_button(ui, theme::Icon::Play, "Resume", true).clicked() {
                actions.push(Action::Resume(view.id));
            }
        }
        JobStatus::Failed => {
            if widgets::icon_button(ui, theme::Icon::Remove, "Remove from list", true).clicked() {
                actions.push(Action::Remove(view.id));
            }
            if widgets::icon_button(ui, theme::Icon::Folder, "Open folder", true).clicked() {
                open_folder(view);
            }
            if view.failure == FailureKind::RemoteChanged {
                let tooltip = "Restart — discard the partial download and start over";
                if widgets::icon_button(ui, theme::Icon::Retry, tooltip, true).clicked() {
                    actions.push(Action::Restart(view.id));
                }
            } else if widgets::icon_button(ui, theme::Icon::Retry, "Retry", true).clicked() {
                actions.push(Action::Retry(view.id));
            }
        }
        JobStatus::Completed => {
            if widgets::icon_button(ui, theme::Icon::Remove, "Remove from list", true).clicked() {
                actions.push(Action::Remove(view.id));
            }
            if widgets::icon_button(ui, theme::Icon::Folder, "Open folder", true).clicked() {
                open_folder(view);
            }
            if widgets::icon_button(ui, theme::Icon::OpenFile, "Open file", true).clicked() {
                RapidGrabApp::open_path(&view.output);
            }
        }
        JobStatus::Cancelled => {
            if widgets::icon_button(ui, theme::Icon::Remove, "Remove from list", true).clicked() {
                actions.push(Action::Remove(view.id));
            }
            if widgets::icon_button(ui, theme::Icon::Folder, "Open folder", true).clicked() {
                open_folder(view);
            }
            if widgets::icon_button(ui, theme::Icon::Play, "Resume", true).clicked() {
                actions.push(Action::Resume(view.id));
            }
        }
    }
}

/// One download card in the queue.
fn job_card(
    ui: &mut egui::Ui,
    view: &JobView,
    actions: &mut Vec<Action>,
    virus_scan_available: bool,
) {
    let name = view_name(&view.url, &view.output);
    let progress = progress_view(view);

    widgets::card().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = theme::SP_XS;

        ui.add(
            egui::Label::new(
                egui::RichText::new(truncate_url(&name, 68))
                    .font(egui::FontId::new(
                        theme::BODY_SIZE,
                        theme::semibold_family(),
                    ))
                    .color(theme::TEXT),
            )
            .truncate(),
        )
        .on_hover_text(format!("{}\n{}", view.url, view.output.display()));

        ui.horizontal(|ui| {
            ui.add(egui::Label::new(theme::caption_tertiary(view_host(
                &view.url,
            ))));
            widgets::chip(ui, view.category.name(), theme::TEXT_SECONDARY);
            // Priority cycles Low → Normal → High → Low.
            let prio_label = format!("{} priority", view.priority.label());
            if widgets::button(ui, &prio_label, widgets::ButtonKind::Secondary, true)
                .on_hover_text("Cycle queue priority (High runs first)")
                .clicked()
            {
                actions.push(Action::CyclePriority(view.id));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::status_chip(ui, &view.status);
            });
        });

        ui.add_space(theme::SP_XS);
        if let Some(fraction) = progress.fraction {
            widgets::progress(
                ui,
                ui.make_persistent_id(("rg-progress", view.id)),
                fraction,
                progress.color,
            );
            ui.add_space(theme::SP_XS);
        }

        if view.status == JobStatus::Completed {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = theme::SP_S;
                match &view.virus {
                    Some(result) => {
                        let color = match result.status {
                            crate::virus::VirusStatus::Clean => theme::SUCCESS,
                            crate::virus::VirusStatus::Threat => theme::ERROR,
                            crate::virus::VirusStatus::Error => theme::WARNING,
                        };
                        widgets::chip(ui, result.status.label(), color)
                            .on_hover_text(&result.message);
                        if virus_scan_available {
                            if widgets::button(
                                ui,
                                "Scan again",
                                widgets::ButtonKind::Secondary,
                                true,
                            )
                            .clicked()
                            {
                                actions.push(Action::VirusCheck(view.id));
                            }
                        }
                    }
                    None => {
                        if virus_scan_available {
                            widgets::chip(ui, "Not scanned", theme::TEXT_SECONDARY);
                            if widgets::button(
                                ui,
                                "Run scan",
                                widgets::ButtonKind::Secondary,
                                true,
                            )
                            .clicked()
                            {
                                actions.push(Action::VirusCheck(view.id));
                            }
                        }
                    }
                }
            });
            if let Some(result) = &view.virus {
                if result.status == crate::virus::VirusStatus::Threat {
                    ui.add_space(theme::SP_XS);
                    widgets::alert(ui, &result.message, theme::ERROR);
                }
            }
            ui.add_space(theme::SP_XS);
        }

        ui.horizontal(|ui| {
            let buttons = action_count(&view.status) as f32;
            let icons_width = buttons * theme::ICON_BTN + (buttons - 1.0).max(0.0) * theme::SP_XS;
            let text_width = (ui.available_width() - icons_width).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(text_width, theme::ICON_BTN),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(egui::Label::new(theme::caption(progress.detail.clone())).truncate());
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                job_actions(ui, view, actions);
            });
        });

        if let Some(error) = &view.error {
            ui.add_space(theme::SP_XS);
            widgets::alert(ui, error, theme::ERROR);
        }
        if let Some(notice) = &view.notice {
            ui.add_space(theme::SP_XS);
            widgets::alert(ui, notice, theme::WARNING);
        }
    });
}

/// Returns an inline validation message for the URL field.
fn url_error(url: &str) -> Option<&'static str> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Some("Use a link that starts with http:// or https://.");
    }
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') || host.contains(' ') || host.contains('\\') {
        return Some("This link is missing a valid host name.");
    }
    None
}

/// Extracts a short source label (domain) from a URL.
fn view_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.trim_start_matches("www.");
    if host.is_empty() {
        url.to_string()
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_duration(Duration::from_secs(5)), "0:05");
        assert_eq!(fmt_duration(Duration::from_secs(65)), "1:05");
        assert_eq!(fmt_duration(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn truncates_long_urls_on_char_boundaries() {
        assert_eq!(truncate_url("short", 10), "short");
        let long = "https://example.com/ünïcode/very/long/path.iso";
        let cut = truncate_url(long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn non_empty_trims_and_drops_blank() {
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty("  abc "), Some("abc".to_string()));
    }

    #[test]
    fn old_settings_gain_resolver_defaults() {
        let old = r#"{
            "max_concurrent": 2,
            "connections": 8,
            "limit_kbps": "",
            "user_agent": "",
            "last_dir": "C:\\Downloads"
        }"#;
        let settings: Settings = serde_json::from_str(old).expect("old settings still load");
        assert!(settings.auto_resolve);
        assert_eq!(settings.max_height, rg_resolve::DEFAULT_MAX_HEIGHT);
        assert!(settings.yt_dlp_path.is_empty());
        assert!(settings.ffmpeg_path.is_empty());
        assert!(settings.yt_dlp_args.is_empty());
    }

    #[test]
    fn labels_media_stages() {
        assert_eq!(media_stage_label(MediaStage::Video), "video");
        assert_eq!(media_stage_label(MediaStage::Audio), "audio");
        assert_eq!(media_stage_label(MediaStage::Merging), "merge");
    }
}
