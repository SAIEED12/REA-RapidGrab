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

    logo: egui::TextureHandle,
}

impl RapidGrabApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::ui::apply_theme(&cc.egui_ctx);
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
                if let Some(started) = job.started_at.take() { job.elapsed += started.elapsed(); }
                job.status = JobStatus::Failed;
                job.error = Some("download worker exited unexpectedly".to_string());
            }
        }
        self.pump_queue();
        let mut actions: Vec<Action> = Vec::new();
        let views = self.job_views();
        let active = views.iter().filter(|v| v.status.is_active()).count();
        let queued = views.iter().filter(|v| v.status == JobStatus::Queued).count();
        let total_speed: f64 = views.iter().filter_map(|v| v.media.as_ref().map(|m| m.speed_bps).or_else(|| v.progress.as_ref().map(|p| p.speed_bps))).sum();
        let summary = if views.is_empty() {
            "Fast, segmented downloads".to_string()
        } else if active > 0 {
            format!(
                "{} active · {} queued · max {}",
                active, queued, self.settings.max_concurrent
            )
        } else if queued > 0 {
            format!("{} queued · max {}", queued, self.settings.max_concurrent)
        } else {
            format!("{} in list · max {}", views.len(), self.settings.max_concurrent)
        };

        // --- header -------------------------------------------------------
        egui::TopBottomPanel::top("rapidgrab_header")
            .frame(crate::ui::panel_frame().inner_margin(egui::Margin {
                left: 20.0,
                right: 20.0,
                top: 14.0,
                bottom: 12.0,
            }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::new(
                            self.logo.id(),
                            egui::vec2(34.0, 34.0),
                        ))
                        .rounding(8.0),
                    );
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(egui::Label::new(crate::ui::title("RapidGrab")));
                        ui.add(egui::Label::new(crate::ui::caption(summary.clone())));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(crate::ui::primary_button(if self.show_add_panel {
                                "Hide"
                            } else {
                                "+ New download"
                            }))
                            .clicked()
                        {
                            self.show_add_panel = !self.show_add_panel;
                        }
                        if active > 0 {
                            ui.add_space(4.0);
                            crate::ui::pill(
                                ui,
                                &format!("{}/s", human_bytes(total_speed.max(0.0) as u64)),
                                crate::ui::ACCENT,
                                true,
                            );
                        }
                    });
                });

                // --- add-download card ------------------------------------
                if self.show_add_panel {
                    ui.add_space(12.0);
                    crate::ui::card_frame().show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.add(egui::Label::new(crate::ui::heading("New download")));
                        ui.add_space(4.0);

                        ui.add(egui::Label::new(crate::ui::caption("URL")));
                        let url_response = ui.add(
                            egui::TextEdit::singleline(&mut self.new_url)
                                .hint_text("https://example.com/file.iso")
                                .desired_width(f32::INFINITY),
                        );
                        if url_response.lost_focus() {
                            self.suggest_output();
                        }

                        ui.add_space(2.0);
                        ui.add(egui::Label::new(crate::ui::caption("Save as")));
                        ui.horizontal(|ui| {
                            let browse_width = 82.0;
                            ui.add(
                                egui::TextEdit::singleline(&mut self.new_output)
                                    .hint_text("C:\\Downloads\\download.bin")
                                    .desired_width((ui.available_width() - browse_width - 8.0).max(120.0)),
                            );
                            if ui.add(crate::ui::ghost_button("Browse…")).clicked() {
                                self.browse_output();
                            }
                        });

                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::Label::new(crate::ui::caption("Connections")));
                            ui.add(egui::Slider::new(&mut self.new_connections, 1..=16));
                            ui.add_space(10.0);
                            ui.add(egui::Label::new(crate::ui::caption("Speed cap")));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.new_limit_kbps)
                                    .hint_text("KB/s")
                                    .desired_width(64.0),
                            );
                            ui.add_space(10.0);
                            ui.add(egui::Label::new(crate::ui::caption("Run at once")));
                            ui.add(egui::Slider::new(&mut self.settings.max_concurrent, 1..=8));
                        });

                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::ui::small("Advanced"))
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 4.0;
                                ui.add(egui::Label::new(crate::ui::caption(
                                    "SHA-256 checksum (optional)",
                                )));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.new_sha256)
                                        .hint_text("expected digest")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.add(egui::Label::new(crate::ui::caption(
                                    "User-Agent (optional)",
                                )));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.new_user_agent)
                                        .hint_text("Mozilla/5.0…")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.add_space(2.0);
                                ui.checkbox(
                                    &mut self.new_restart,
                                    "Restart — ignore existing parts",
                                );
                            });

                        egui::CollapsingHeader::new(crate::ui::small("Media resolver"))
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 4.0;
                                ui.checkbox(
                                    &mut self.settings.auto_resolve,
                                    "Resolve video pages automatically with yt-dlp",
                                );
                                ui.horizontal(|ui| {
                                    ui.add(egui::Label::new(crate::ui::caption(
                                        "Max height",
                                    )));
                                    ui.add(
                                        egui::DragValue::new(&mut self.settings.max_height)
                                            .speed(10.0)
                                            .range(0..=4320)
                                            .suffix("p"),
                                    );
                                    ui.add(egui::Label::new(crate::ui::caption(
                                        "0 = best available",
                                    )));
                                });
                                ui.add(egui::Label::new(crate::ui::caption("yt-dlp path")));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.yt_dlp_path)
                                        .hint_text("auto-detected")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.add(egui::Label::new(crate::ui::caption("ffmpeg path")));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.ffmpeg_path)
                                        .hint_text("auto-detected")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.add(egui::Label::new(crate::ui::caption(
                                    "Extra yt-dlp args",
                                )));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.yt_dlp_args)
                                        .hint_text("--cookies \"C:\\Users\\me\\cookies.txt\"")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.add(egui::Label::new(crate::ui::caption(
                                    "Leave paths blank to search PATH, pip, WinGet and Scoop.",
                                )));
                            });

                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if self.settings.auto_resolve {
                                ui.add(egui::Label::new(crate::ui::caption(
                                    "Video pages are resolved with yt-dlp; direct file URLs skip it.",
                                )));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let can_start = !self.new_url.trim().is_empty()
                                        && !self.new_output.trim().is_empty();
                                    if ui
                                        .add_enabled(
                                            can_start,
                                            crate::ui::primary_button("Start download"),
                                        )
                                        .clicked()
                                    {
                                        self.add_download();
                                    }
                                },
                            );
                        });
                    });
                }
            });

        // --- downloads ----------------------------------------------------
        egui::CentralPanel::default()
            .frame(crate::ui::panel_frame().inner_margin(egui::Margin {
                left: 20.0,
                right: 20.0,
                top: 16.0,
                bottom: 16.0,
            }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(crate::ui::heading("Downloads")));
                    if !views.is_empty() {
                        ui.add(egui::Label::new(crate::ui::small(format!(
                            "{active} active · {queued} queued"
                        ))));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(crate::ui::caption(format!(
                            "max {} at once",
                            self.settings.max_concurrent
                        ))));
                    });
                });
                ui.add_space(10.0);

                if views.is_empty() {
                    ui.add_space(72.0);
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Label::new(crate::ui::heading("No downloads yet")));
                        ui.add(egui::Label::new(crate::ui::small(
                            "Paste a file URL or a video page link to get started.",
                        )));
                        ui.add_space(10.0);
                        if ui
                            .add(crate::ui::primary_button("+ New download"))
                            .clicked()
                        {
                            self.show_add_panel = true;
                        }
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for view in &views {
                            job_card(ui, view, &mut actions);
                            ui.add_space(10.0);
                        }
                    });
            });

        self.handle_actions(actions);
        self.pump_queue();

        // Repaint while anything is in flight so timers stay live.
        if self
            .jobs
            .values()
            .any(|j| j.status == JobStatus::Pausing)
        {
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

/// One download card in the queue.
fn job_card(ui: &mut egui::Ui, view: &JobView, actions: &mut Vec<Action>) {
    use crate::ui;

    ui::card_frame().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;

        // Title row: file name, tooltip with the full source, status pill.
        ui.horizontal(|ui| {
            let name = view_name(&view.url, &view.output);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(truncate_url(&name, 72))
                        .size(13.0)
                        .strong(),
                ),
            )
            .on_hover_text(format!("{}\n{}", view.url, view.output.display()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui::status_pill(ui, &view.status);
            });
        });

        ui.add(egui::Label::new(ui::caption(format!(
            "#{}  ·  {}",
            view.id,
            truncate_url(&view.output.display().to_string(), 110)
        ))));

        // Progress.
        if view.status == JobStatus::Completed {
            let size = std::fs::metadata(&view.output)
                .map(|m| m.len())
                .unwrap_or_default();
            ui::progress_bar(
                ui,
                1.0,
                &format!("done · {}", human_bytes(size)),
                ui::GREEN,
            );
        } else if let Some(media) = &view.media {
            let frac = media.overall.clamp(0.0, 1.0) as f32;
            let stage = media_stage_label(media.stage);
            let text = if media.stage == MediaStage::Merging {
                format!("merging audio + video · {:.0}%", frac * 100.0)
            } else if media.total > 0 {
                format!(
                    "{:.0}%  ·  {} / {}  ·  {}",
                    frac * 100.0,
                    human_bytes(media.downloaded),
                    human_bytes(media.total),
                    stage
                )
            } else {
                format!("{stage} · {:.0}%", frac * 100.0)
            };
            ui::progress_bar(ui, frac, &text, ui::ACCENT);
        } else if let Some(p) = view.progress.as_ref() {
            let frac = if p.total > 0 {
                (p.downloaded as f64 / p.total as f64).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            let text = if p.total > 0 {
                format!(
                    "{:.0}%  ·  {} / {}",
                    frac * 100.0,
                    human_bytes(p.downloaded),
                    human_bytes(p.total)
                )
            } else {
                format!("{} received", human_bytes(p.downloaded))
            };
            ui::progress_bar(ui, frac, &text, ui::ACCENT);
        }

        // Stats line.
        let mut stats = String::new();
        match view.status {
            JobStatus::Completed => {
                stats.push_str(&format!(
                    "{} conns · {} elapsed",
                    view.connections,
                    fmt_duration(view.elapsed)
                ));
            }
            _ => {
                let speed = view
                    .media
                    .as_ref()
                    .map(|m| m.speed_bps)
                    .or_else(|| view.progress.as_ref().map(|p| p.speed_bps))
                    .unwrap_or(0.0);
                if speed > 1.0 {
                    stats.push_str(&format!("{}/s · ", human_bytes(speed as u64)));
                }
                let conns = view
                    .progress
                    .as_ref()
                    .map(|p| p.connections)
                    .unwrap_or(view.connections);
                stats.push_str(&format!("{conns} conns · {}", fmt_duration(view.elapsed)));
                if let Some(p) = view.progress.as_ref() {
                    if p.speed_bps > 1.0 && p.total > p.downloaded && p.total > 0 {
                        let secs = (p.total - p.downloaded) as f64 / p.speed_bps;
                        stats.push_str(&format!(
                            " · ETA {}",
                            fmt_duration(Duration::from_secs_f64(secs.max(0.0)))
                        ));
                    }
                }
                if let Some(limit) = view.speed_limit {
                    stats.push_str(&format!(" · cap {}/s", human_bytes(limit)));
                }
            }
        }
        ui.add(egui::Label::new(ui::small(stats)));

        // Feedback.
        if let Some(err) = &view.error {
            ui.add_space(2.0);
            ui::alert_bar(ui, err, ui::RED, "!");
        }
        if let Some(notice) = &view.notice {
            ui.add_space(2.0);
            ui::alert_bar(ui, notice, ui::AMBER, "i");
        }

        ui.add_space(2.0);

        // Actions.
        ui.horizontal(|ui| {
            let folder = |view: &JobView| {
                let dir = view
                    .output
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                RapidGrabApp::open_path(&dir);
            };

            match view.status {
                JobStatus::Queued => {
                    if ui.add(ui::text_button("Start now")).clicked() {
                        actions.push(Action::StartNow(view.id));
                    }
                    if ui.add(ui::destructive_button("Remove")).clicked() {
                        actions.push(Action::Remove(view.id));
                    }
                }
                JobStatus::Starting
                | JobStatus::Downloading
                | JobStatus::Resolving
                | JobStatus::Merging => {
                    if ui.add(ui::text_button("Pause")).clicked() {
                        actions.push(Action::Pause(view.id));
                    }
                    if ui.add(ui::destructive_button("Cancel")).clicked() {
                        actions.push(Action::Stop(view.id));
                    }
                }
                JobStatus::Pausing => {
                    ui.add(egui::Label::new(ui::caption("stopping…")));
                }
                JobStatus::Paused => {
                    if ui.add(ui::text_button("Resume")).clicked() {
                        actions.push(Action::Resume(view.id));
                    }
                    if ui.add(ui::destructive_button("Remove")).clicked() {
                        actions.push(Action::Remove(view.id));
                    }
                }
                JobStatus::Failed => {
                    if ui.add(ui::text_button("Retry")).clicked() {
                        actions.push(Action::Retry(view.id));
                    }
                    if ui.add(ui::ghost_button("Open folder")).clicked() {
                        folder(view);
                    }
                    if ui.add(ui::destructive_button("Remove")).clicked() {
                        actions.push(Action::Remove(view.id));
                    }
                }
                JobStatus::Completed => {
                    if ui.add(ui::success_button("Open file")).clicked() {
                        RapidGrabApp::open_path(&view.output);
                    }
                    if ui.add(ui::ghost_button("Open folder")).clicked() {
                        folder(view);
                    }
                    if ui.add(ui::destructive_button("Remove")).clicked() {
                        actions.push(Action::Remove(view.id));
                    }
                }
                JobStatus::Cancelled => {
                    if ui.add(ui::text_button("Resume")).clicked() {
                        actions.push(Action::Resume(view.id));
                    }
                    if ui.add(ui::ghost_button("Open folder")).clicked() {
                        folder(view);
                    }
                    if ui.add(ui::destructive_button("Remove")).clicked() {
                        actions.push(Action::Remove(view.id));
                    }
                }
            }
        });
    });
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
