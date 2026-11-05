use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use rg_core::{Download, Progress};
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

#[derive(Debug, Clone)]
pub struct DownloadUpdate {
    pub id: usize,
    /// Incremented on every (re)start; stale updates are ignored.
    pub run: u64,
    pub progress: Option<Progress>,
    pub status: JobStatus,
    pub error: Option<String>,
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
    pub restart: bool,
    pub status: JobStatus,
    pub progress: Option<Progress>,
    pub media: Option<MediaProgress>,
    pub error: Option<String>,
    /// Informational message shown under the status (for example a skipped
    /// checksum); cleared when the job starts again.
    pub notice: Option<String>,
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
    notice: Option<String>,
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
    Remove(usize),
}

pub struct RapidGrabApp {
    rt: Arc<Runtime>,
    update_rx: Receiver<DownloadUpdate>,
    update_tx: Sender<DownloadUpdate>,
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
    show_add_panel: bool,
    /// Set when a button should move keyboard focus back to the URL field.
    focus_url: bool,
    /// Cached result of the "output path already exists" probe.
    output_probe: Option<OutputProbe>,
    /// Per-launch salt so the page starts at the top instead of restoring the
    /// previous session's scroll offset.
    page_nonce: u64,

    logo: egui::TextureHandle,
}

/// Cached filesystem probe for the Save-as path, refreshed at most every 500 ms.
struct OutputProbe {
    path: String,
    checked: Instant,
    exists: bool,
}

impl RapidGrabApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::ui::apply_theme(&cc.egui_ctx);
        // A dark title bar keeps the native frame consistent with the app.
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
        let rt = Arc::new(Runtime::new().expect("Failed to create tokio runtime"));
        let (update_tx, update_rx) = unbounded();

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
            show_add_panel: true,
            focus_url: false,
            output_probe: None,
            page_nonce: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            logo: crate::icon::logo_texture(&cc.egui_ctx),
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

        let id = self.next_id;
        self.next_id += 1;

        let speed_limit = self
            .new_limit_kbps
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&x| x > 0)
            .map(|x| x * 1024);
        let sha256 = non_empty(&self.new_sha256);
        let user_agent = non_empty(&self.new_user_agent);

        self.jobs.insert(
            id,
            DownloadJob {
                id,
                run: 0,
                url: self.new_url.trim().to_string(),
                output: PathBuf::from(self.new_output.trim()),
                connections: self.new_connections.max(1),
                speed_limit,
                sha256,
                user_agent,
                restart: self.new_restart,
                status: JobStatus::Queued,
                progress: None,
                media: None,
                error: None,
                notice: None,
                started_at: None,
                elapsed: Duration::ZERO,
                cancel: Arc::new(AtomicBool::new(false)),
                intent: None,
                remove_after_stop: false,
                handle: None,
            },
        );

        // Remember the form as the next default.
        self.settings.connections = self.new_connections;
        self.settings.limit_kbps = self.new_limit_kbps.clone();
        self.settings.user_agent = self.new_user_agent.clone();

        self.new_url.clear();
        self.new_sha256.clear();
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
            job.notice = None;
            job.progress = None;
            job.media = None;
            job.intent = None;
            job.remove_after_stop = false;
            job.started_at = Some(Instant::now());
            (
                job.run,
                job.url.clone(),
                job.output.clone(),
                job.connections,
                job.speed_limit,
                job.sha256.clone(),
                job.user_agent.clone(),
                job.restart,
            )
        }) else {
            return;
        };

        let (run, url, output, connections, speed_limit, sha256, user_agent, restart) = params;
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
                    output,
                    media,
                    notice: None,
                });
            };

            let mut dl = Download::new(url.clone(), output.clone())
                .connections(connections)
                .resume(!restart)
                .cancel_flag(task_cancel.clone());

            if let Some(limit) = speed_limit {
                dl = dl.speed_limit(limit);
            }
            if let Some(ref expected) = sha256 {
                dl = dl.sha256(expected.clone());
            }
            if let Some(ref ua) = user_agent {
                dl = dl.user_agent(ua.clone());
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
                            MediaEvent::Resolving => {
                                (JobStatus::Resolving, None, None, None)
                            }
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

    fn resume_job(&mut self, id: usize) {
        if let Some(job) = self.jobs.get_mut(&id) {
            if matches!(job.status, JobStatus::Paused | JobStatus::Cancelled | JobStatus::Failed) {
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
        let active = self
            .jobs
            .values()
            .filter(|j| j.status.is_active())
            .count();
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

        match update.status {
            JobStatus::Starting
            | JobStatus::Downloading
            | JobStatus::Resolving
            | JobStatus::Merging => {
                if job.status != JobStatus::Pausing {
                    job.status = update.status;
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
            }
            _ => {}
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
                notice: j.notice.clone(),
                elapsed: j.total_elapsed(),
            })
            .collect();
        views.sort_by_key(|v| v.id);
        views
    }
}

impl eframe::App for RapidGrabApp {
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
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.update_rx.try_recv() {
            self.apply_update(update);
        }
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

        // Esc drops keyboard focus from the URL field (and any other input).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            ctx.memory_mut(|m| m.stop_text_input());
        }

        let mut actions: Vec<Action> = Vec::new();
        let views = self.job_views();
        let active = views.iter().filter(|v| v.status.is_active()).count();
        let queued = views.iter().filter(|v| v.status == JobStatus::Queued).count();
        let total_speed: f64 = views
            .iter()
            .filter_map(|v| {
                v.media
                    .as_ref()
                    .map(|m| m.speed_bps)
                    .or_else(|| v.progress.as_ref().map(|p| p.speed_bps))
            })
            .sum();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 20.0,
                        right: 20.0,
                        top: 14.0,
                        bottom: 12.0,
                    }),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_source(("rapidgrab-page", self.page_nonce))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
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
            .rounding(theme::R_MEDIUM),
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

    // Space between the field and the button; the field width is derived
    // from the same gap so the row spans exactly the available width.
    let gap = theme::SP_S;

    // The button's real painted width (label + 2×SP_L), floored at BROWSE_W
    // so it never renders narrower than intended. field_w is computed from
    // the *same* number the button paints, so the row's right edge is
    // exactly the card's inner right edge — same as the URL field and the
    // right-aligned Start button.
    let button_w = widgets::button_width_for(ui, "Browse…").max(theme::BROWSE_W);
    let field_w = (ui.available_width() - button_w - gap).max(140.0);

    ui.horizontal(|ui| {
        widgets::text_field(
            ui,
            widgets::Field::new("rg-output", "C:\\Downloads\\download.bin").width(field_w),
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
    });
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
                widgets::stepper(
                    ui,
                    "rg-concurrent",
                    &mut app.settings.max_concurrent,
                    1..=8,
                );
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
            widgets::stepper(
                ui,
                "rg-concurrent",
                &mut app.settings.max_concurrent,
                1..=8,
            );
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
        ui.add_space(theme::SP_XS);

        ui.add(egui::Label::new(theme::label("URL")));
        if std::mem::take(&mut app.focus_url) {
            ui.memory_mut(|m| m.request_focus(egui::Id::new("rg-url")));
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
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-sha", "expected digest"),
                        sha,
                    );
                },
                |ui| {
                    ui.add(egui::Label::new(theme::label("User-Agent (optional)")));
                    widgets::text_field(
                        ui,
                        widgets::Field::new("rg-ua", "Mozilla/5.0…"),
                        ua,
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

        if app.settings.auto_resolve {
            widgets::info_line(
                ui,
                "Video pages are resolved with yt-dlp; direct file URLs skip it.",
            );
        }
        ui.add_space(theme::SP_XS);

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_start = url_error(&app.new_url).is_none()
                    && !app.new_url.trim().is_empty();
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
        if let Some(index) = widgets::dropdown(
            ui,
            "rg-max-height",
            selected,
            &labels,
            theme::DROPDOWN_W,
        ) {
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
        if media.speed_bps > 1.0 {
            detail.push_str(&format!(" · {}/s", human_bytes(media.speed_bps as u64)));
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
        if progress.speed_bps > 1.0 {
            detail.push_str(&format!(
                " · {}/s",
                human_bytes(progress.speed_bps as u64)
            ));
        }
        if progress.speed_bps > 1.0 && progress.total > progress.downloaded {
            let secs = (progress.total - progress.downloaded) as f64 / progress.speed_bps;
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
            if widgets::icon_button(ui, theme::Icon::Retry, "Retry", true).clicked() {
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
                egui::Id::new(("rg-progress", view.id)),
                fraction,
                progress.color,
            );
            ui.add_space(theme::SP_XS);
        }

        ui.horizontal(|ui| {
            let icons_width = 3.0 * theme::ICON_BTN + 3.0 * theme::SP_S;
            let text_width = (ui.available_width() - icons_width).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(text_width, theme::ICON_BTN),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(theme::caption(progress.detail.clone()))
                            .truncate(),
                    );
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
