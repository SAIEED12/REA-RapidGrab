//! Orchestration: resolve a page, download its streams through `rg-core`, and
//! merge them when the site serves video and audio separately.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rg_core::{Download, Error as CoreError, NetworkConfig, Progress};

use crate::error::{ResolveError, Result};
use crate::metadata::{self, SelectedStream, Selection};
use crate::tools::{Runner, SystemRunner, Tools};

/// Everything needed for one media download.
#[derive(Debug, Clone)]
pub struct MediaRequest {
    /// The page URL the user pasted (for example a YouTube `/watch` link).
    pub page_url: String,
    /// Requested save path; `download.bin` and extensionless paths are replaced
    /// with the sanitized video title and the real container extension.
    pub output: PathBuf,
    pub connections: usize,
    pub speed_limit: Option<u64>,
    /// Highest video height to accept; `None` means best available.
    pub max_height: Option<u32>,
    /// Start the streams from scratch instead of resuming.
    pub restart: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    /// Extra arguments passed to `yt-dlp` (for example cookies).
    pub extra_args: Vec<String>,
    /// Proxy settings applied to every stream download.
    pub network: NetworkConfig,
}

impl MediaRequest {
    pub fn new(page_url: impl Into<String>, output: impl Into<PathBuf>) -> Self {
        Self {
            page_url: page_url.into(),
            output: output.into(),
            connections: rg_core::DEFAULT_CONNECTIONS,
            speed_limit: None,
            max_height: Some(crate::DEFAULT_MAX_HEIGHT),
            restart: false,
            cancel: None,
            extra_args: Vec::new(),
            network: NetworkConfig::default(),
        }
    }

    pub fn connections(mut self, connections: usize) -> Self {
        self.connections = connections.max(1);
        self
    }

    pub fn speed_limit(mut self, bytes_per_sec: u64) -> Self {
        self.speed_limit = Some(bytes_per_sec);
        self
    }

    pub fn max_height(mut self, height: u32) -> Self {
        self.max_height = Some(height);
        self
    }

    pub fn restart(mut self, restart: bool) -> Self {
        self.restart = restart;
        self
    }

    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn extra_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.extra_args = args.into_iter().collect();
        self
    }

    /// Proxy settings for the stream downloads (not for yt-dlp itself).
    pub fn network(mut self, network: NetworkConfig) -> Self {
        self.network = network;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaStage {
    Video,
    Audio,
    Merging,
}

#[derive(Debug, Clone)]
pub struct MediaProgress {
    pub stage: MediaStage,
    /// Bytes downloaded for the current stage.
    pub downloaded: u64,
    /// Expected bytes for the current stage (0 when unknown).
    pub total: u64,
    pub speed_bps: f64,
    /// Combined 0.0–1.0 progress across both streams and the merge.
    pub overall: f64,
}

#[derive(Debug, Clone)]
pub enum MediaEvent {
    Resolving,
    Resolved {
        title: String,
        ext: String,
        height: Option<u32>,
        needs_merge: bool,
        total_bytes: Option<u64>,
        /// Final path (title-derived when the request used `download.bin`).
        output: PathBuf,
    },
    Progress(MediaProgress),
    Merging,
    Done {
        output: PathBuf,
        bytes: u64,
    },
}

type EventFn = Arc<dyn Fn(MediaEvent) + Send + Sync>;

const VIDEO_SHARE: f64 = 0.80;
const AUDIO_START: f64 = 0.80;
const AUDIO_END: f64 = 0.97;

/// Resolve and download `request` using the system tools.
pub async fn download_media(
    tools: &Tools,
    request: MediaRequest,
    on_event: impl Fn(MediaEvent) + Send + Sync + 'static,
) -> Result<PathBuf> {
    download_media_with(&SystemRunner, tools, request, on_event).await
}

/// Testable variant that accepts an injected [`Runner`].
pub async fn download_media_with(
    runner: &dyn Runner,
    tools: &Tools,
    request: MediaRequest,
    on_event: impl Fn(MediaEvent) + Send + Sync + 'static,
) -> Result<PathBuf> {
    check_cancel(&request.cancel)?;
    let on_event: EventFn = Arc::new(on_event);
    on_event(MediaEvent::Resolving);

    let resolved = metadata::resolve(
        runner,
        &tools.yt_dlp,
        &request.page_url,
        request.max_height,
        &request.extra_args,
    )
    .await?;
    let selection = resolved.selection.clone();

    let output = final_output_path(&request.output, &resolved.title, &selection.ext);
    on_event(MediaEvent::Resolved {
        title: resolved.title.clone(),
        ext: selection.ext.clone(),
        height: selection.video.height,
        needs_merge: selection.needs_merge,
        total_bytes: selection.total_bytes,
        output: output.clone(),
    });

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    if !selection.needs_merge {
        let stats = run_stream(
            &request,
            &selection.video,
            &output,
            MediaStage::Video,
            0.0,
            1.0,
            &format!("{}|video|{}", request.page_url, selection.video.format_id),
            &on_event,
        )
        .await?;
        on_event(MediaEvent::Done {
            output: stats.output.clone(),
            bytes: stats.total,
        });
        return Ok(stats.output);
    }

    let ffmpeg = tools.ffmpeg.as_ref().ok_or(ResolveError::FfmpegMissing)?;
    let audio = selection
        .audio
        .as_ref()
        .ok_or(ResolveError::NoStream { kind: "audio" })?;

    let video_tmp = sidecar_path(&output, "rgvideo", &selection.video.ext);
    let audio_tmp = sidecar_path(&output, "rgaudio", &audio.ext);

    let video_stats = run_stream(
        &request,
        &selection.video,
        &video_tmp,
        MediaStage::Video,
        0.0,
        VIDEO_SHARE,
        &format!("{}|video|{}", request.page_url, selection.video.format_id),
        &on_event,
    )
    .await?;

    check_cancel(&request.cancel)?;

    let audio_stats = run_stream(
        &request,
        audio,
        &audio_tmp,
        MediaStage::Audio,
        AUDIO_START,
        AUDIO_END - AUDIO_START,
        &format!("{}|audio|{}", request.page_url, audio.format_id),
        &on_event,
    )
    .await?;

    check_cancel(&request.cancel)?;
    on_event(MediaEvent::Merging);
    on_event(MediaEvent::Progress(MediaProgress {
        stage: MediaStage::Merging,
        downloaded: 0,
        total: 0,
        speed_bps: 0.0,
        overall: AUDIO_END,
    }));

    merge(
        runner,
        ffmpeg,
        &video_stats.output,
        &audio_stats.output,
        &output,
        request.cancel.as_ref(),
        &selection,
    )
    .await?;

    cleanup_stream(&video_stats.output).await;
    cleanup_stream(&audio_stats.output).await;

    let bytes = tokio::fs::metadata(&output)
        .await
        .map(|meta| meta.len())
        .unwrap_or(video_stats.total + audio_stats.total);

    on_event(MediaEvent::Done {
        output: output.clone(),
        bytes,
    });
    Ok(output)
}

/// Choose the final filename.
///
/// An explicit filename with a real extension is respected. A placeholder
/// (`download.bin`) or extensionless path becomes `<sanitized title>.<ext>`,
/// and an existing file is never overwritten: ` (1)`, ` (2)`, … are tried.
pub fn final_output_path(requested: &Path, title: &str, ext: &str) -> PathBuf {
    let is_placeholder = match requested.extension().and_then(|e| e.to_str()) {
        None => true,
        Some(ext) => ext.is_empty() || ext.eq_ignore_ascii_case("bin"),
    };
    if !is_placeholder {
        return requested.to_path_buf();
    }

    let parent = requested.parent().unwrap_or_else(|| Path::new(""));
    let base = rg_core::sanitize_filename(title).unwrap_or_else(|| "video".to_string());
    let ext = if ext.trim().is_empty() {
        "mp4"
    } else {
        ext.trim()
    };
    unique_path(parent.join(format!("{base}.{ext}")))
}

fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video")
        .to_string();
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 1..10_000 {
        let name = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// Stable sidecar path so a cancelled download can resume into the same file.
fn sidecar_path(output: &Path, tag: &str, ext: &str) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new(""));
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let ext = if ext.trim().is_empty() {
        "bin"
    } else {
        ext.trim()
    };
    parent.join(format!(".{stem}.{tag}.{ext}"))
}

#[allow(clippy::too_many_arguments)]
async fn run_stream(
    request: &MediaRequest,
    stream: &SelectedStream,
    output: &Path,
    stage: MediaStage,
    overall_start: f64,
    overall_span: f64,
    resume_key: &str,
    on_event: &EventFn,
) -> Result<rg_core::Stats> {
    check_cancel(&request.cancel)?;

    let mut dl = Download::new(stream.url.clone(), output.to_path_buf())
        .connections(request.connections)
        .resume(!request.restart)
        .network(request.network.clone())
        .resume_key(resume_key);

    if let Some(limit) = request.speed_limit {
        dl = dl.speed_limit(limit);
    }
    if let Some(ua) = &stream.user_agent {
        dl = dl.user_agent(ua.clone());
    }
    if let Some(referer) = &stream.referer {
        dl = dl.referer(referer.clone());
    } else {
        dl = dl.referer(request.page_url.clone());
    }
    if let Some(cancel) = &request.cancel {
        dl = dl.cancel_flag(cancel.clone());
    }

    let callback = Arc::clone(on_event);
    dl = dl.on_progress(move |p: Progress| {
        let fraction = if p.total > 0 {
            (p.downloaded as f64 / p.total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        callback(MediaEvent::Progress(MediaProgress {
            stage,
            downloaded: p.downloaded,
            total: p.total,
            speed_bps: p.speed_bps,
            overall: (overall_start + overall_span * fraction).clamp(0.0, 1.0),
        }));
    });

    Ok(dl.run().await?)
}

async fn merge(
    runner: &dyn Runner,
    ffmpeg: &Path,
    video: &Path,
    audio: &Path,
    output: &Path,
    cancel: Option<&Arc<AtomicBool>>,
    selection: &Selection,
) -> Result<()> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        video.display().to_string(),
        "-i".to_string(),
        audio.display().to_string(),
        "-c".to_string(),
        "copy".to_string(),
    ];
    if selection.ext.eq_ignore_ascii_case("mp4") || selection.ext.eq_ignore_ascii_case("mov") {
        args.push("-movflags".to_string());
        args.push("+faststart".to_string());
    }
    args.push(output.display().to_string());

    let run = runner.run(ffmpeg, &args);
    tokio::pin!(run);

    let finished = match cancel {
        Some(flag) => {
            tokio::select! {
                result = &mut run => Some(result),
                _ = wait_cancelled(flag) => None,
            }
        }
        None => Some(run.await),
    };

    let Some(finished) = finished else {
        let _ = tokio::fs::remove_file(output).await;
        return Err(ResolveError::Core(CoreError::Cancelled));
    };

    let result = finished?;
    if !result.success {
        let _ = tokio::fs::remove_file(output).await;
        let message = result.stderr_text();
        return Err(ResolveError::Tool {
            tool: "ffmpeg".to_string(),
            message: if message.is_empty() {
                format!("exit status {:?}", result.status)
            } else {
                message
            },
        });
    }
    Ok(())
}

async fn cleanup_stream(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
    let _ = tokio::fs::remove_dir_all(rg_core::parts_dir_for(path)).await;
}

async fn wait_cancelled(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn check_cancel(flag: &Option<Arc<AtomicBool>>) -> Result<()> {
    if flag
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        return Err(ResolveError::Core(CoreError::Cancelled));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rg-resolve-media-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn replaces_the_bin_placeholder_with_the_title() {
        let dir = temp_dir("name");
        let final_path = final_output_path(&dir.join("download.bin"), "Walk Through Text", "mp4");
        assert_eq!(final_path, dir.join("Walk Through Text.mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_overwrites_an_existing_title_file() {
        let dir = temp_dir("unique");
        std::fs::write(dir.join("Clip.mp4"), b"existing").expect("write existing");
        let final_path = final_output_path(&dir.join("download.bin"), "Clip", "mp4");
        assert_eq!(final_path, dir.join("Clip (1).mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_an_explicit_filename() {
        let dir = temp_dir("explicit");
        let requested = dir.join("my-video.mkv");
        assert_eq!(final_output_path(&requested, "Ignored", "mp4"), requested);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_the_title() {
        let dir = temp_dir("sanitize");
        let final_path =
            final_output_path(&dir.join("download.bin"), "A/B: \"quoted\" <clip>?", "mp4");
        let name = final_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(name, "A B quoted clip.mp4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_names_are_stable_and_temporary() {
        let output = Path::new("C:/Users/me/Desktop/Clip.mp4");
        assert_eq!(
            sidecar_path(output, "rgvideo", "mp4"),
            PathBuf::from("C:/Users/me/Desktop/.Clip.rgvideo.mp4")
        );
        assert_eq!(
            sidecar_path(output, "rgaudio", "m4a"),
            PathBuf::from("C:/Users/me/Desktop/.Clip.rgaudio.m4a")
        );
    }
}
