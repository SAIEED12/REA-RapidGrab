use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use rg_core::{Download, Progress};
use tokio::runtime::Runtime;

#[derive(Debug, Clone)]
pub struct DownloadUpdate {
    pub id: usize,
    pub progress: Option<Progress>,
    pub status: JobStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Starting,
    Downloading,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone)]
pub struct DownloadJob {
    pub id: usize,
    pub url: String,
    pub output: PathBuf,
    pub connections: usize,
    pub speed_limit: Option<u64>,
    pub status: JobStatus,
    pub progress: Option<Progress>,
    pub error: Option<String>,
    pub started_at: Instant,
    pub cancel: Arc<AtomicBool>,
}

pub struct RapidGrabApp {
    rt: Arc<Runtime>,
    update_rx: Receiver<DownloadUpdate>,
    update_tx: Sender<DownloadUpdate>,
    jobs: HashMap<usize, DownloadJob>,
    next_id: usize,

    // Add form
    new_url: String,
    new_output: String,
    new_connections: usize,
    new_limit_kbps: String,
    new_restart: bool,
    new_sha256: String,
    new_user_agent: String,
    show_add_panel: bool,

    // UI state
    selected_job: Option<usize>,
}

impl RapidGrabApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let rt = Arc::new(Runtime::new().expect("Failed to create tokio runtime"));
        let (update_tx, update_rx) = unbounded();

        Self {
            rt,
            update_rx,
            update_tx,
            jobs: HashMap::new(),
            next_id: 1,
            new_url: String::new(),
            new_output: String::new(),
            new_connections: 8,
            new_limit_kbps: String::new(),
            new_restart: false,
            new_sha256: String::new(),
            new_user_agent: String::new(),
            show_add_panel: true,
            selected_job: None,
        }
    }

    fn add_download(&mut self) {
        if self.new_url.is_empty() || self.new_output.is_empty() {
            return;
        }

        let id = self.next_id;
        self.next_id += 1;

        let output_path = PathBuf::from(&self.new_output);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let tx = self.update_tx.clone();

        let url = self.new_url.clone();
        let connections = self.new_connections;
        let speed_limit = self.new_limit_kbps.parse::<u64>().ok().filter(|&x| x > 0).map(|x| x * 1024);
        let restart = self.new_restart;
        let sha256 = if self.new_sha256.is_empty() { None } else { Some(self.new_sha256.clone()) };
        let user_agent = if self.new_user_agent.is_empty() { None } else { Some(self.new_user_agent.clone()) };

        let job = DownloadJob {
            id,
            url: url.clone(),
            output: output_path.clone(),
            connections,
            speed_limit,
            status: JobStatus::Starting,
            progress: None,
            error: None,
            started_at: Instant::now(),
            cancel: cancel.clone(),
        };
        self.jobs.insert(id, job);

        self.rt.spawn(async move {
            let _ = tx.send(DownloadUpdate { id, progress: None, status: JobStatus::Starting, error: None });
            
            let mut dl = Download::new(url, output_path.clone()).connections(connections).resume(!restart);
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
                let _ = tx2.send(DownloadUpdate { id, progress: Some(p.clone()), status: JobStatus::Downloading, error: None });
            });

            let result = dl.run().await;
            if cancel_clone.load(Ordering::Relaxed) {
                let _ = tx.send(DownloadUpdate { id, progress: None, status: JobStatus::Cancelled, error: None });
            } else {
                match result {
                    Ok(_) => {
                        let _ = tx.send(DownloadUpdate { id, progress: None, status: JobStatus::Completed, error: None });
                    }
                    Err(e) => {
                        let _ = tx.send(DownloadUpdate { id, progress: None, status: JobStatus::Failed, error: Some(e.to_string()) });
                    }
                }
            }
        });

        // Clear form
        self.new_url.clear();
        self.new_sha256.clear();
    }

    fn browse_output(&mut self) {
        if let Some(path) = rfd::FileDialog::new().set_file_name("download.bin").save_file() {
            self.new_output = path.to_string_lossy().to_string();
        }
    }
}

impl eframe::App for RapidGrabApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process updates
        while let Ok(update) = self.update_rx.try_recv() {
            if let Some(job) = self.jobs.get_mut(&update.id) {
                if let Some(ref p) = update.progress {
                    job.progress = Some(p.clone());
                }
                job.status = update.status.clone();
                if let Some(err) = update.error {
                    job.error = Some(err);
                }
            }
        }

        // Top panel - Add download
        egui::TopBottomPanel::top("add_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RapidGrab");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(if self.show_add_panel { "Hide" } else { "Add Download" }).clicked() {
                        self.show_add_panel = !self.show_add_panel;
                    }
                });
            });
            if self.show_add_panel {
                ui.separator();
                egui::Grid::new("add_grid").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                    ui.label("URL:");
                    ui.text_edit_singleline(&mut self.new_url);
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
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.new_restart, "Restart (ignore existing parts)");
                    if ui.button("Start Download").clicked() {
                        self.add_download();
                    }
                });
                ui.separator();
            }
        });

        // Main panel - Downloads list
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Downloads");
            if self.jobs.is_empty() {
                ui.label("No downloads yet. Add one above.");
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut jobs_to_remove: Vec<usize> = Vec::new();
                let mut job_ids: Vec<usize> = self.jobs.keys().cloned().collect();
                job_ids.sort();

                for id in job_ids {
                    if let Some(job) = self.jobs.get(&id).cloned() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("#{} ", job.id));
                                let filename = job.output.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
                                ui.strong(filename);
                                ui.label(format!("({})", job.status_text()));
                            });
                            ui.label(format!("URL: {}", truncate_url(&job.url, 60)));
                            
                            if let Some(ref p) = job.progress {
                                let pct = if p.total > 0 { (p.downloaded as f64 / p.total as f64 * 100.0) } else { 0.0 };
                                let prog = if p.total > 0 { p.downloaded as f32 / p.total as f32 } else { 0.0 };
                                ui.add(egui::ProgressBar::new(prog).text(format!("{:.1}% ({:.2}/{:.2} MB)", pct, p.downloaded as f64/1_048_576.0, p.total as f64/1_048_576.0)));
                                ui.label(format!("Speed: {:.2} MB/s | Connections: {}", p.speed_bps/1_048_576.0, p.connections));
                            } else if job.status == JobStatus::Completed {
                                ui.label("Completed");
                            } else if job.status == JobStatus::Starting {
                                ui.label("Starting...");
                            }

                            if let Some(ref err) = job.error {
                                ui.colored_label(egui::Color32::RED, format!("Error: {}", err));
                            }

                            ui.horizontal(|ui| {
                                if job.status == JobStatus::Downloading {
                                    if ui.button("Cancel").clicked() {
                                        job.cancel.store(true, Ordering::Relaxed);
                                    }
                                }
                                if job.status == JobStatus::Completed || job.status == JobStatus::Failed || job.status == JobStatus::Cancelled {
                                    if ui.button("Remove").clicked() {
                                        jobs_to_remove.push(id);
                                    }
                                    if ui.button("Open Folder").clicked() {
                                        if let Some(parent) = job.output.parent() {
                                            let _ = open::that(parent);
                                        }
                                    }
                                    if ui.button("Open File").clicked() && job.status == JobStatus::Completed {
                                        let _ = open::that(&job.output);
                                    }
                                }
                            });
                        });
                        ui.add_space(4.0);
                    }
                }
                for id in jobs_to_remove {
                    self.jobs.remove(&id);
                }
            });
        });

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

impl DownloadJob {
    fn status_text(&self) -> &'static str {
        match self.status {
            JobStatus::Starting => "Starting",
            JobStatus::Downloading => "Downloading",
            JobStatus::Completed => "Completed",
            JobStatus::Cancelled => "Cancelled",
            JobStatus::Failed => "Failed",
        }
    }
}

fn truncate_url(url: &str, max: usize) -> String {
    if url.len() <= max { url.to_string() } else { format!("{}...", &url[..max-3]) }
}
