use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use rg_core::{Download, Progress};
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
    Pausing,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    /// A job that owns a running tokio task.
    fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Downloading | Self::Pausing)
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Starting => "Starting…",
            Self::Downloading => "Downloading",
            Self::Pausing => "Pausing…",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
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
    pub error: Option<String>,
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
    error: Option<String>,
    elapsed: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    max_concurrent: usize,
    connections: usize,
    limit_kbps: String,
    user_agent: String,
    last_dir: String,
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
                error: None,
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

        let Some(params) = self.jobs.get_mut(&id).map(|job| {
            job.run += 1;
            job.cancel = cancel.clone();
            job.status = JobStatus::Starting;
            job.error = None;
            job.progress = None;
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
            let send = |progress: Option<Progress>, status: JobStatus, error: Option<String>| {
                let _ = tx.send(DownloadUpdate {
                    id,
                    run,
                    progress,
                    status,
                    error,
                });
            };

            let mut dl = Download::new(url, output)
                .connections(connections)
                .resume(!restart)
                .cancel_flag(task_cancel);

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
                let _ = tx2.send(DownloadUpdate {
                    id,
                    run,
                    progress: Some(p),
                    status: JobStatus::Downloading,
                    error: None,
                });
            });

            match dl.run().await {
                Ok(_) => send(None, JobStatus::Completed, None),
                Err(rg_core::Error::Cancelled) => send(None, JobStatus::Cancelled, None),
                Err(e) => send(None, JobStatus::Failed, Some(e.to_string())),
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
                JobStatus::Starting | JobStatus::Downloading | JobStatus::Pausing => {
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

        match update.status {
            JobStatus::Starting | JobStatus::Downloading => {
                if job.status != JobStatus::Pausing {
                    job.status = update.status;
                }
                if let Some(p) = update.progress {
                    job.progress = Some(p);
                }
            }
            JobStatus::Cancelled => {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                job.progress = None;
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
                job.status = JobStatus::Completed;
                job.error = None;
            }
            JobStatus::Failed => {
                if let Some(started) = job.started_at.take() {
                    job.elapsed += started.elapsed();
                }
                job.progress = None;
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
                error: j.error.clone(),
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
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Drain engine updates.
        while let Ok(update) = self.update_rx.try_recv() {
            self.apply_update(update);
        }

        // 2. Watchdog: a task that vanished without a terminal message.
        for job in self.jobs.values_mut() {
            if job.status.is_active() {
                if let Some(handle) = &job.handle {
                    if handle.is_finished() {
                        if let Some(started) = job.started_at.take() {
                            job.elapsed += started.elapsed();
                        }
                        job.status = JobStatus::Failed;
                        job.error = Some("download worker exited unexpectedly".to_string());
                    }
                }
            }
        }

        // 3. Keep the queue moving.
        self.pump_queue();

        let mut actions: Vec<Action> = Vec::new();

        // --- top panel: branding + add form -------------------------------
        egui::TopBottomPanel::top("add_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::new(
                        self.logo.id(),
                        egui::vec2(26.0, 26.0),
                    ))
                    .rounding(6.0),
                );
                ui.add_space(4.0);
                ui.heading("RapidGrab");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(if self.show_add_panel { "Hide" } else { "Add Download" })
                        .clicked()
                    {
                        self.show_add_panel = !self.show_add_panel;
                    }
                });
            });

            if self.show_add_panel {
                ui.separator();
                egui::Grid::new("add_grid")
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("URL:");
                        if ui.text_edit_singleline(&mut self.new_url).lost_focus() {
                            self.suggest_output();
                        }
                        ui.end_row();

                        ui.label("Save as:");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.new_output);
                            if ui.button("Browse...").clicked() {
                                self.browse_output();
                            }
                        });
                        ui.end_row();

                        ui.label("Connections:");
                        ui.add(egui::Slider::new(&mut self.new_connections, 1..=16));
                        ui.end_row();

                        ui.label("Speed limit (KB/s):");
                        ui.text_edit_singleline(&mut self.new_limit_kbps);
                        ui.end_row();

                        ui.label("SHA256 (optional):");
                        ui.text_edit_singleline(&mut self.new_sha256);
                        ui.end_row();

                        ui.label("User-Agent (optional):");
                        ui.text_edit_singleline(&mut self.new_user_agent);
                        ui.end_row();

                        ui.label("Run at once:");
                        ui.add(egui::Slider::new(
                            &mut self.settings.max_concurrent,
                            1..=8,
                        ));
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.new_restart, "Restart (ignore existing parts)");
                    let can_start =
                        !self.new_url.trim().is_empty() && !self.new_output.trim().is_empty();
                    if ui
                        .add_enabled(can_start, egui::Button::new("Start Download"))
                        .clicked()
                    {
                        self.add_download();
                    }
                    if self.new_restart {
                        ui.label(
                            egui::RichText::new("restart discards resumable parts")
                                .small()
                                .weak(),
                        );
                    }
                });
                ui.separator();
            }
        });

        // --- central panel: job list --------------------------------------
        let views = self.job_views();
        let active = views.iter().filter(|v| v.status.is_active()).count();
        let queued = views
            .iter()
            .filter(|v| v.status == JobStatus::Queued)
            .count();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Downloads");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{active} active · {queued} queued · max {}",
                            self.settings.max_concurrent
                        ))
                        .small()
                        .weak(),
                    );
                });
            });

            if views.is_empty() {
                ui.label("No downloads yet. Add one above.");
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for view in &views {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("#{} ", view.id));
                            ui.strong(truncate_url(&view.url, 60));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(view.status.label())
                                            .small()
                                            .weak(),
                                    );
                                },
                            );
                        });

                        ui.label(
                            egui::RichText::new(view.output.display().to_string())
                                .small()
                                .weak(),
                        );

                        match &view.progress {
                            Some(p) if p.total > 0 => {
                                let frac = (p.downloaded as f32 / p.total as f32).clamp(0.0, 1.0);
                                ui.add(
                                    egui::ProgressBar::new(frac).text(format!(
                                        "{} / {}  ({:.0}%)",
                                        human_bytes(p.downloaded),
                                        human_bytes(p.total),
                                        frac * 100.0
                                    )),
                                );
                                let eta = if p.speed_bps > 1.0 && p.total > p.downloaded {
                                    let secs =
                                        (p.total - p.downloaded) as f64 / p.speed_bps;
                                    format!(" · ETA {}", fmt_duration(Duration::from_secs_f64(secs.max(0.0))))
                                } else {
                                    String::new()
                                };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}/s · {} conns · {}{eta}",
                                        human_bytes(p.speed_bps.max(0.0) as u64),
                                        p.connections,
                                        fmt_duration(view.elapsed),
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                            _ => {
                                let mut line = format!(
                                    "{} conns · elapsed {}",
                                    view.connections,
                                    fmt_duration(view.elapsed)
                                );
                                if let Some(limit) = view.speed_limit {
                                    line.push_str(&format!(
                                        " · cap {}/s",
                                        human_bytes(limit)
                                    ));
                                }
                                ui.label(egui::RichText::new(line).small().weak());
                            }
                        }

                        if let Some(err) = &view.error {
                            ui.label(egui::RichText::new(err).small().color(egui::Color32::LIGHT_RED));
                        }

                        ui.horizontal(|ui| {
                            match view.status {
                                JobStatus::Queued => {
                                    if ui.button("Start now").clicked() {
                                        actions.push(Action::StartNow(view.id));
                                    }
                                    if ui.button("Cancel").clicked() {
                                        actions.push(Action::Remove(view.id));
                                    }
                                }
                                JobStatus::Starting | JobStatus::Downloading => {
                                    if ui.button("Pause").clicked() {
                                        actions.push(Action::Pause(view.id));
                                    }
                                    if ui.button("Cancel").clicked() {
                                        actions.push(Action::Stop(view.id));
                                    }
                                }
                                JobStatus::Pausing => {
                                    ui.label(egui::RichText::new("stopping…").small().weak());
                                }
                                JobStatus::Paused => {
                                    if ui.button("Resume").clicked() {
                                        actions.push(Action::Resume(view.id));
                                    }
                                    if ui.button("Cancel").clicked() {
                                        actions.push(Action::Stop(view.id));
                                    }
                                }
                                JobStatus::Failed => {
                                    if ui.button("Retry").clicked() {
                                        actions.push(Action::Retry(view.id));
                                    }
                                    if ui.button("Open Folder").clicked() {
                                        let dir = view
                                            .output
                                            .parent()
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or_else(|| PathBuf::from("."));
                                        Self::open_path(&dir);
                                    }
                                }
                                JobStatus::Completed => {
                                    if ui.button("Open File").clicked() {
                                        Self::open_path(&view.output);
                                    }
                                    if ui.button("Open Folder").clicked() {
                                        let dir = view
                                            .output
                                            .parent()
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or_else(|| PathBuf::from("."));
                                        Self::open_path(&dir);
                                    }
                                }
                                JobStatus::Cancelled => {
                                    if ui.button("Resume").clicked() {
                                        actions.push(Action::Resume(view.id));
                                    }
                                    if ui.button("Open Folder").clicked() {
                                        let dir = view
                                            .output
                                            .parent()
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or_else(|| PathBuf::from("."));
                                        Self::open_path(&dir);
                                    }
                                }
                            }

                            let removable = view.status.is_terminal()
                                || matches!(view.status, JobStatus::Paused | JobStatus::Queued);
                            if removable && ui.button("Remove").clicked() {
                                actions.push(Action::Remove(view.id));
                            }
                        });

                        if matches!(view.status, JobStatus::Pausing) {
                            ctx.request_repaint();
                        }
                    });
                }
            });
        });

        self.handle_actions(actions);
        self.pump_queue();

        // Repaint while anything is in flight so timers stay live.
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
}
