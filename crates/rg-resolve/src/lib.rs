//! Optional media resolution for web pages that do not expose direct files.
//!
//! RapidGrab remains the downloader. [`media::download_media`] resolves a page
//! URL with an external `yt-dlp` (only to obtain signed stream URLs and a
//! title), downloads every stream through [`rg_core::Download`], and uses
//! `ffmpeg` only when a separate audio track must be muxed into the video.
//!
//! [`pair::download_pair`] is the same video+audio muxing for streams the
//! browser captured separately (Facebook's DASH renditions).

pub mod error;
pub mod media;
pub mod metadata;
pub mod pair;
pub mod tools;

pub use error::{ResolveError, Result};
pub use media::{
    download_media, download_media_with, final_output_path, merge_streams, MediaEvent,
    MediaProgress, MediaRequest, MediaStage,
};
pub use metadata::{
    resolve, select_streams, Format, Metadata, Resolved, SelectedStream, Selection,
};
pub use pair::{download_pair, download_pair_with, PairRequest};
pub use tools::{find_tool, CommandOutput, Runner, SystemRunner, Tools};

/// Default resolution cap used by the front ends: 1080p H.264 is broadly
/// playable and avoids multi-gigabyte 4K AV1 downloads.
pub const DEFAULT_MAX_HEIGHT: u32 = 1080;
