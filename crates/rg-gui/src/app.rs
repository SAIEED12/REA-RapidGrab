use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use rg_core::{Credentials, Download, NetworkConfig, Progress, ProxyConfig, ProxyMode};
use rg_resolve::{MediaEvent, MediaProgress, MediaRequest, MediaStage, Tools};
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
}

fn default_auto_resolve() -> bool {
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
}

pub struct RapidGrabApp {
    rt: Arc<Runtime>,
    update_rx: Receiver<DownloadUpdate>,
    update_tx: Sender<DownloadUpdate>,
    capture_rx: Receiver<rg_bridge::CaptureRequest>,
    probe_rx: Receiver<(String, Option<u64>, Option<String>)>,
    probe_tx: Sender<(String, Option<u64>, Option<String>)>,
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
    output: String,
    dont_ask_again: bool,
    action: Option<PopupAction>,
}

impl PopupShared {
    fn blank() -> Self {
        Self {
            filename: String::new(),
            url: String::new(),
            size_label: String::new(),
            type_label: String::new(),
            source_label: String::new(),
            output: String::new(),
            dont_ask_again: false,
            action: None,
        }
    }
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
    size: Option<u64>,
    content_type: Option<String>,
    /// Set when the probe request is still in flight.
    probing: bool,
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
        let (probe_tx, probe_rx) = unbounded::<(String, Option<u64>, Option<String>)>();
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
        let settings = Settings {
            max_concurrent: settings.max_concurrent.clamp(1, 8),
            connections: settings.connections.clamp(1, 16),
            ..settings
        };

        Self {
            rt,
            update_rx,
            update_tx,
            capture_rx,
            probe_rx,
            probe_tx,
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
            logo: crate::icon::logo_texture(&cc.egui_ctx),
        }
    }

    /// Drains the capture/probe channels and builds or updates the pending
    /// capture. Safe to call from both `logic()` (hidden window) and `ui()`.
    fn handle_captures(&mut self) {
        // Browser captures: intercept = Download popup, click/link = pre-fill.
        while let Ok(capture) = self.capture_rx.try_recv() {
            // Best filename: Content-Disposition header wins, then the
            // extension's guess, then the tab title for YouTube.
            let filename = capture
                .content_disposition_filename
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or(capture.filename.as_deref().filter(|s| !s.trim().is_empty()));
            let suggested = suggested_output_for(&self.settings.last_dir, filename, &capture.url);

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
                    );
                }
                self.show_add_panel = true;
                continue;
            }

            // Always pre-fill the form as a fallback (confirm-first backup).
            self.new_url = capture.url.clone();
            if let Some(ref name) = filename {
                self.new_output = suggested.clone();
                let _ = name;
            }
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
                );
            } else {
                let size = capture.content_length;
                let content_type = capture.content_type.clone();
                let probing = size.is_none() && content_type.is_none();
                self.pending_capture = Some(PendingCapture {
                    capture: capture.clone(),
                    output: suggested,
                    filename: filename.unwrap_or(&capture.url).to_string(),
                    size,
                    content_type,
                    probing,
                    dont_ask_again: false,
                });
                if probing {
                    self.spawn_probe(capture);
                }
            }
            self.show_add_panel = true;
            self.focus_url = true;
        }
        while let Ok((url, size, ctype)) = self.probe_rx.try_recv() {
            if let Some(pending) = &mut self.pending_capture {
                if pending.capture.url == url {
                    pending.size = size;
                    pending.content_type = ctype;
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
        let dir = if self.settings.last_dir.is_empty() {
            dirs::download_dir().unwrap_or_else(|| PathBuf::from("."))
        } else {
            PathBuf::from(&self.settings.last_dir)
        };
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
                    let _ = tx.send((capture.url.clone(), None, None));
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
            let (size, ctype) = match tokio::time::timeout(Duration::from_secs(10), req.send()).await {
                Ok(Ok(resp)) if resp.status().is_success() => {
                    let size = resp
                        .headers()
                        .get(reqwest::header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());
                    let ctype = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    (size, ctype)
                }
                _ => (None, None),
            };
            let _ = tx.send((capture.url.clone(), size, ctype));
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
        let output = pending.output.clone();
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
            },
        );
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
        let max = self.settings.max_concurrent.max(1);
        let active = self.jobs.values().filter(|j| j.status.is_active()).count();
        if active >= max {
            return;
        }

        let mut queued: Vec<usize> = self
            .jobs
            .iter()
            .filter(|(_, j)| j.status == JobStatus::Queued)
            .map(|(&id, _)| id)
            .collect();
        queued.sort_unstable();

        for id in queued.into_iter().take(max - active) {
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
                job.progress = None;
                job.media = None;
                let intent = job.intent.take();
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
                job.progress = None;
                job.media = None;
                job.status = JobStatus::Completed;
                job.error = None;
            }
            JobStatus::Failed => {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                job.progress = None;
                job.media = None;
                job.status = JobStatus::Failed;
                job.error = update.error;
                job.failure = update.failure;
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

    fn handle_actions(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Pause(id) => self.pause_job(id),
                Action::Stop(id) => self.stop_job(id),
                Action::StartNow(id) => self.start_now(id),
                Action::Resume(id) => self.resume_job(id),
                Action::Retry(id) => self.resume_job(id),
                Action::Restart(id) => self.restart_job(id),
                Action::Remove(id) => self.remove_job(id),
            }
        }
    }

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
            let (size_label, type_label) = if is_media_page {
                ("Video page".to_string(), "resolved on Start".to_string())
            } else {
                let size_label = match pending.size {
                    Some(bytes) => human_bytes(bytes),
                    None if pending.probing => "Resolving\u{2026}".to_string(),
                    None => "Unknown".to_string(),
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
                .with_title("Download")
                .with_inner_size([420.0, 360.0])
                .with_resizable(false)
                .with_active(true),
            move |ui, _class| {
                let mut state = shared.lock().unwrap();
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    state.action = Some(PopupAction::Cancel);
                }
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::default()
                            .fill(theme::CARD)
                            .inner_margin(egui::Margin::same(16)),
                    )
                    .show(ui, |ui: &mut egui::Ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(&state.filename)
                                .strong()
                                .size(theme::SECTION_SIZE),
                        ));
                        ui.add(egui::Label::new(theme::caption(format!(
                            "{} \u{b7} {} \u{b7} {}",
                            state.source_label, state.size_label, state.type_label
                        ))));
                        ui.add(egui::Label::new(theme::caption(state.url.clone())));
                        ui.add(egui::Label::new(theme::label("Save as")));
                        ui.horizontal(|ui| {
                            let field_w =
                                (ui.available_width() - theme::BROWSE_W - theme::SP_S).max(120.0);
                            widgets::text_field(
                                ui,
                                widgets::Field::new(
                                    "rg-popup-output",
                                    "Choose where to save\u{2026}",
                                )
                                .width(field_w),
                                &mut state.output,
                            );
                            if widgets::button(
                                ui,
                                "Browse\u{2026}",
                                widgets::ButtonKind::Secondary,
                                true,
                            )
                            .clicked()
                            {
                                state.action = Some(PopupAction::Browse);
                            }
                        });

                        widgets::checkbox(
                            ui,
                            "rg-popup-dontask",
                            &mut state.dont_ask_again,
                            "Don't ask again for this site",
                        );

                        ui.horizontal(|ui| {
                            if widgets::button(ui, "Cancel", widgets::ButtonKind::Secondary, true)
                                .clicked()
                            {
                                state.action = Some(PopupAction::Cancel);
                            }
                            if widgets::button(
                                ui,
                                "Download Later",
                                widgets::ButtonKind::Secondary,
                                true,
                            )
                            .clicked()
                            {
                                state.action = Some(PopupAction::Later);
                            }
                            if widgets::button(
                                ui,
                                "Start download",
                                widgets::ButtonKind::Primary,
                                true,
                            )
                            .clicked()
                            {
                                state.action = Some(PopupAction::Start);
                            }
                        });
                    });
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
            pending.output = state.output.clone();
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
                        let name = std::path::Path::new(&pending.filename)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "download.bin".to_string());
                        pending.output = dir.join(name).display().to_string();
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
        while let Ok(update) = self.update_rx.try_recv() {
            self.apply_update(update);
        }
        self.handle_captures();
        self.pump_queue();
        self.show_capture_popup(ctx);
        if self.pending_capture.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
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
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(update) = self.update_rx.try_recv() {
            self.apply_update(update);
        }
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
                        header(ui, self, active, queued, total_speed);
                        ui.add_space(theme::SP_M);

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

/// `<last dir>/<filename or name from url>`, mirroring `suggest_output`.
fn suggested_output_for(last_dir: &str, filename: Option<&str>, url: &str) -> String {
    let name = filename
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            url.split(['?', '#'])
                .next()
                .unwrap_or("")
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("download.bin")
                .to_string()
        });
    let dir = if last_dir.is_empty() {
        dirs::download_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(last_dir)
    };
    dir.join(name).display().to_string()
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

    for view in views {
        job_card(ui, view, actions);
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
fn job_card(ui: &mut egui::Ui, view: &JobView, actions: &mut Vec<Action>) {
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
