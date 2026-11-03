//! Optional media resolution for web pages that do not expose direct files.
//!
//! RapidGrab remains the downloader: [`download_media`] resolves page URLs with
//! an external `yt-dlp` (only to obtain signed stream URLs and a title), then
//! downloads every stream through [`rg_core::Download`]. `ffmpeg` is used only
//! when a separate audio track has to be muxed into the video.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error(
        "yt-dlp was not found; install it (for example `python -m pip install --user yt-dlp`) \
         or set its path in Settings"
    )]
    YtDlpMissing,

    #[error(
        "ffmpeg is required to merge this video's audio track; install it or set its path in Settings"
    )]
    FfmpegMissing,

    #[error("configured {tool} path does not exist: {path}")]
    BadToolPath { tool: &'static str, path: String },

    #[error("could not run {tool}: {source}")]
    Spawn {
        tool: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{tool} failed: {message}")]
    Tool { tool: String, message: String },

    #[error("yt-dlp found no usable {kind} stream")]
    NoStream { kind: &'static str },

    #[error("yt-dlp returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Core(#[from] rg_core::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ResolveError>;

impl ResolveError {
    /// True when the caller asked to stop.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, ResolveError::Core(rg_core::Error::Cancelled))
    }
}
