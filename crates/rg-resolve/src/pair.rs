//! Download a browser-captured video track together with its sibling audio
//! DASH rendition (Facebook Reels, …) and mux them with ffmpeg.
//!
//! The extension hands over both track URLs when the player streamed them
//! separately; the single-URL [`rg_core::Download`] path cannot combine
//! them. Everything here mirrors the resolver's stream handling so a
//! cancelled run can resume into the same sidecar files.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rg_core::{Download, NetworkConfig, Progress};

use crate::error::Result;
use crate::media::{self, merge_streams, MediaEvent, MediaProgress, MediaStage};
use crate::tools::{Runner, SystemRunner};

/// Everything needed to fetch and mux one captured video + audio pair.
#[derive(Debug, Clone)]
pub struct PairRequest {
    /// Page URL the user asked to download (used as `Referer` fallback).
    pub page_url: String,
    pub video_url: String,
    pub audio_url: String,
    /// Final save path chosen in the popup.
    pub output: PathBuf,
    pub connections: usize,
    pub speed_limit: Option<u64>,
    /// Start streams from scratch instead of resuming.
    pub restart: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    pub network: NetworkConfig,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub cookie: Option<String>,
    /// Cookies scoped to the audio host; falls back to the video cookies.
    pub audio_cookie: Option<String>,
}

impl PairRequest {
    pub fn new(page_url: impl Into<String>, video_url: impl Into<String>,
               audio_url: impl Into<String>, output: impl Into<PathBuf>) -> Self {
        Self {
            page_url: page_url.into(),
            video_url: video_url.into(),
            audio_url: audio_url.into(),
            output: output.into(),
            connections: rg_core::DEFAULT_CONNECTIONS,
            speed_limit: None,
            restart: false,
            cancel: None,
            network: NetworkConfig::default(),
            user_agent: None,
            referer: None,
            cookie: None,
            audio_cookie: None,
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

    pub fn restart(mut self, restart: bool) -> Self {
        self.restart = restart;
        self
    }

    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn network(mut self, network: NetworkConfig) -> Self {
        self.network = network;
        self
    }

    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    pub fn referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }

    pub fn cookie(mut self, cookie: impl Into<String>) -> Self {
        self.cookie = Some(cookie.into());
        self
    }

    pub fn audio_cookie(mut self, cookie: impl Into<String>) -> Self {
        self.audio_cookie = Some(cookie.into());
        self
    }
}

const VIDEO_SHARE: f64 = 0.80;
const AUDIO_START: f64 = 0.80;
const AUDIO_END: f64 = 0.97;

type EventFn = Arc<dyn Fn(MediaEvent) + Send + Sync>;

/// Download the pair with the system tools.
pub async fn download_pair(
    ffmpeg: &Path,
    request: PairRequest,
    on_event: impl Fn(MediaEvent) + Send + Sync + 'static,
) -> Result<PathBuf> {
    download_pair_with(&SystemRunner, ffmpeg, request, on_event).await
}

/// Testable variant that accepts an injected [`Runner`].
pub async fn download_pair_with(
    runner: &dyn Runner,
    ffmpeg: &Path,
    request: PairRequest,
    on_event: impl Fn(MediaEvent) + Send + Sync + 'static,
) -> Result<PathBuf> {
    media::check_cancel(&request.cancel)?;
    let on_event: EventFn = Arc::new(on_event);

    if let Some(parent) = request.output.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let video_ext = extension_from_url(&request.video_url).unwrap_or("mp4".to_string());
    let audio_ext = extension_from_url(&request.audio_url).unwrap_or("m4a".to_string());
    let video_tmp = media::sidecar_path(&request.output, "rgvideo", &video_ext);
    let audio_tmp = media::sidecar_path(&request.output, "rgaudio", &audio_ext);

    tracing::debug!(
        "paired media download: video={video_tmp:?} audio={audio_tmp:?} output={:?}",
        request.output
    );

    let video_stats = run_track(
        &request,
        &request.video_url,
        &video_tmp,
        MediaStage::Video,
        0.0,
        VIDEO_SHARE,
        &format!("{}|video|{}", request.page_url, request.video_url),
        request.cookie.as_deref(),
        &on_event,
    )
    .await?;

    media::check_cancel(&request.cancel)?;

    let audio_stats = run_track(
        &request,
        &request.audio_url,
        &audio_tmp,
        MediaStage::Audio,
        AUDIO_START,
        AUDIO_END - AUDIO_START,
        &format!("{}|audio|{}", request.page_url, request.audio_url),
        request.audio_cookie.as_deref().or(request.cookie.as_deref()),
        &on_event,
    )
    .await?;

    media::check_cancel(&request.cancel)?;
    on_event(MediaEvent::Merging);
    on_event(MediaEvent::Progress(MediaProgress {
        stage: MediaStage::Merging,
        downloaded: 0,
        total: 0,
        speed_bps: 0.0,
        overall: AUDIO_END,
    }));

    let output_ext = request
        .output
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let faststart = output_ext.is_empty()
        || output_ext == "mp4"
        || output_ext == "mov";
    merge_streams(
        runner,
        ffmpeg,
        &video_stats.output,
        &audio_stats.output,
        &request.output,
        faststart,
        request.cancel.as_ref(),
    )
    .await?;

    cleanup_stream(&video_stats.output).await;
    cleanup_stream(&audio_stats.output).await;

    let bytes = tokio::fs::metadata(&request.output)
        .await
        .map(|meta| meta.len())
        .unwrap_or(video_stats.total + audio_stats.total);

    on_event(MediaEvent::Done {
        output: request.output.clone(),
        bytes,
    });
    Ok(request.output)
}

/// Straight path extension of an http URL, e.g. `.mp4` -> `mp4`; `None` when
/// the path carries no usable extension.
fn extension_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    let dot = last.rfind('.')?;
    if dot == 0 || dot + 1 >= last.len() {
        return None;
    }
    let ext = &last[dot + 1..];
    if ext.len() > 8 || !ext.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

async fn cleanup_stream(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
    let _ = tokio::fs::remove_dir_all(rg_core::parts_dir_for(path)).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_track(
    request: &PairRequest,
    url: &str,
    sidecar: &Path,
    stage: MediaStage,
    overall_start: f64,
    overall_span: f64,
    resume_key: &str,
    cookie: Option<&str>,
    on_event: &EventFn,
) -> Result<rg_core::Stats> {
    media::check_cancel(&request.cancel)?;

    let mut dl = Download::new(url.to_string(), sidecar.to_path_buf())
        .connections(request.connections)
        .resume(!request.restart)
        .network(request.network.clone())
        .resume_key(resume_key);

    if let Some(limit) = request.speed_limit {
        dl = dl.speed_limit(limit);
    }
    if let Some(ua) = &request.user_agent {
        dl = dl.user_agent(ua.clone());
    }
    let referer = request
        .referer
        .clone()
        .unwrap_or_else(|| request.page_url.clone());
    dl = dl.referer(referer);
    if let Some(cookie) = cookie {
        dl = dl.cookie(cookie.to_string());
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
