//! `yt-dlp -J` metadata parsing and stream selection.
//!
//! The JSON output is the stable interface; we never scrape yt-dlp's
//! human-readable table. Selection prefers a single muxed MP4 when it is close
//! to the best resolution, otherwise the best H.264 video-only stream plus the
//! original-language M4A audio so `ffmpeg -c copy` can mux them.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::{ResolveError, Result};
use crate::tools::Runner;

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub webpage_url: Option<String>,
    #[serde(default)]
    pub formats: Vec<Format>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Format {
    #[serde(default)]
    pub format_id: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub vcodec: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub tbr: Option<f64>,
    #[serde(default)]
    pub abr: Option<f64>,
    #[serde(default)]
    pub filesize: Option<u64>,
    #[serde(default)]
    pub filesize_approx: Option<u64>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub format_note: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub http_headers: Option<HashMap<String, String>>,
}

impl Format {
    fn size(&self) -> Option<u64> {
        self.filesize.or(self.filesize_approx)
    }

    fn has_video(&self) -> bool {
        self.vcodec
            .as_deref()
            .is_some_and(|c| !c.is_empty() && c != "none")
    }

    fn has_audio(&self) -> bool {
        self.acodec
            .as_deref()
            .is_some_and(|c| !c.is_empty() && c != "none")
    }

    fn is_direct_http(&self) -> bool {
        let Some(url) = self.url.as_deref() else {
            return false;
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return false;
        }
        // HLS/DASH manifests cannot be fed to the byte-range engine.
        !self
            .protocol
            .as_deref()
            .is_some_and(|p| p.contains("m3u8") || p.contains("dash"))
    }

    fn header(&self, name: &str) -> Option<String> {
        self.http_headers.as_ref().and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        })
    }
}

#[derive(Debug, Clone)]
pub struct SelectedStream {
    pub format_id: String,
    pub url: String,
    pub ext: String,
    pub height: Option<u32>,
    pub filesize: Option<u64>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
}

impl SelectedStream {
    fn from_format(format: &Format) -> Option<Self> {
        Some(Self {
            format_id: format.format_id.clone(),
            url: format.url.clone()?,
            ext: format.ext.clone(),
            height: format.height,
            filesize: format.size(),
            user_agent: format.header("User-Agent"),
            referer: format.header("Referer"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub video: SelectedStream,
    /// Separate audio track to mux; `None` for a muxed stream or a silent video.
    pub audio: Option<SelectedStream>,
    /// Container extension for the final file.
    pub ext: String,
    /// Total bytes when yt-dlp reported sizes for every selected stream.
    pub total_bytes: Option<u64>,
    /// True when `audio` is present.
    pub needs_merge: bool,
    /// True when the best available stream has no audio at all.
    pub silent: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub title: String,
    pub duration: Option<f64>,
    pub selection: Selection,
}

/// Run `yt-dlp -J` and select streams.
pub async fn resolve(
    runner: &dyn Runner,
    yt_dlp: &Path,
    page_url: &str,
    max_height: Option<u32>,
    extra_args: &[String],
) -> Result<Resolved> {
    let mut args = vec![
        "-J".to_string(),
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
    ];
    args.extend(extra_args.iter().cloned());
    args.push(page_url.to_string());

    let output = runner.run(yt_dlp, &args).await?;
    if !output.success {
        let mut message = output.stderr_text();
        if message.is_empty() {
            message = format!("exit status {:?}", output.status);
        }
        let lower = message.to_ascii_lowercase();
        if lower.contains("video is unavailable")
            || lower.contains("video unavailable")
            || lower.contains("sign in to confirm")
            || lower.contains("cookies")
        {
            message.push_str(
                "\nhint: if the video plays in your browser it may require sign-in. \
                 Close the browser, then add `--cookies-from-browser chrome` (or edge/firefox) \
                 to the extra yt-dlp arguments, or pass an exported cookies.txt.",
            );
        }
        return Err(ResolveError::Tool {
            tool: "yt-dlp".to_string(),
            message,
        });
    }

    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let selection = select_streams(&metadata, max_height)?;
    Ok(Resolved {
        title: if metadata.title.trim().is_empty() {
            "video".to_string()
        } else {
            metadata.title.trim().to_string()
        },
        duration: metadata.duration,
        selection,
    })
}

/// Pick the best downloadable streams from `yt-dlp` metadata.
pub fn select_streams(metadata: &Metadata, max_height: Option<u32>) -> Result<Selection> {
    let cap = max_height.unwrap_or(0);
    let within_cap =
        |format: &Format| cap == 0 || format.height.map(|height| height <= cap).unwrap_or(true);

    let rank_video = |format: &Format| -> (u32, u8, u64) {
        let height = format.height.unwrap_or(0);
        let avc = format
            .vcodec
            .as_deref()
            .map(|codec| u8::from(codec.starts_with("avc1")))
            .unwrap_or(0);
        (height, avc, format.tbr.unwrap_or(0.0) as u64)
    };
    let rank_audio = |format: &Format, preferred_ext: &str| -> (u8, u8, u64) {
        let note = format
            .format_note
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let language = format
            .language
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let original = u8::from(
            note.contains("original") || language.is_empty() || language.starts_with("en"),
        );
        let container = u8::from(format.ext.eq_ignore_ascii_case(preferred_ext));
        (original, container, format.abr.unwrap_or(0.0) as u64)
    };

    let mut videos: Vec<&Format> = metadata
        .formats
        .iter()
        .filter(|f| f.is_direct_http() && f.has_video() && !f.has_audio() && within_cap(f))
        .collect();
    videos.sort_by_key(|f| rank_video(f));

    let mut muxed: Vec<&Format> = metadata
        .formats
        .iter()
        .filter(|f| {
            f.is_direct_http()
                && f.has_video()
                && f.has_audio()
                && f.ext.eq_ignore_ascii_case("mp4")
                && within_cap(f)
        })
        .collect();
    muxed.sort_by_key(|f| rank_video(f));

    let best_video = videos.last().copied();
    let best_muxed = muxed.last().copied();

    let muxed_is_close = match (best_muxed, best_video) {
        (Some(muxed), Some(video)) => {
            let muxed_height = muxed.height.unwrap_or(0);
            let video_height = video.height.unwrap_or(0);
            muxed_height + 120 >= video_height
        }
        (Some(_), None) => true,
        _ => false,
    };

    if muxed_is_close {
        let muxed = best_muxed.expect("checked above");
        let selected =
            SelectedStream::from_format(muxed).ok_or(ResolveError::NoStream { kind: "video" })?;
        return Ok(Selection {
            ext: "mp4".to_string(),
            total_bytes: selected.filesize,
            video: selected,
            audio: None,
            needs_merge: false,
            silent: false,
        });
    }

    let video = best_video.ok_or(ResolveError::NoStream { kind: "video" })?;
    let video =
        SelectedStream::from_format(video).ok_or(ResolveError::NoStream { kind: "video" })?;

    // Prefer audio whose container can be copied into the selected video
    // container without re-encoding (AAC in MP4, Opus in WebM).
    let preferred_audio_ext = if video.ext.eq_ignore_ascii_case("webm") {
        "webm"
    } else {
        "m4a"
    };
    let mut audios: Vec<&Format> = metadata
        .formats
        .iter()
        .filter(|f| f.is_direct_http() && !f.has_video() && f.has_audio())
        .collect();
    audios.sort_by_key(|f| rank_audio(f, preferred_audio_ext));
    let audio = audios.last().copied().and_then(SelectedStream::from_format);

    let ext = match audio.as_ref() {
        Some(_) if video.ext.eq_ignore_ascii_case("webm") => "webm".to_string(),
        Some(_) if video.ext.eq_ignore_ascii_case("mp4") => "mp4".to_string(),
        Some(_) => video.ext.clone(),
        None => video.ext.clone(),
    };
    let total_bytes = match (video.filesize, audio.as_ref().and_then(|a| a.filesize)) {
        (Some(video), Some(audio)) => Some(video + audio),
        (Some(video), None) if audio.is_none() => Some(video),
        _ => None,
    };

    Ok(Selection {
        needs_merge: audio.is_some(),
        silent: audio.is_none(),
        ext,
        total_bytes,
        video,
        audio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(json: &str) -> Metadata {
        serde_json::from_str(json).expect("fixture parses")
    }

    fn video(format_id: &str, ext: &str, codec: &str, height: u32, url: &str) -> String {
        format!(
            r#"{{"format_id":"{format_id}","ext":"{ext}","vcodec":"{codec}",
                "acodec":"none","height":{height},"tbr":4000,
                "url":"{url}","protocol":"https"}}"#
        )
    }

    fn muxed(format_id: &str, height: u32, url: &str) -> String {
        format!(
            r#"{{"format_id":"{format_id}","ext":"mp4","vcodec":"avc1.640028",
                "acodec":"mp4a.40.2","height":{height},"tbr":2000,
                "url":"{url}","protocol":"https"}}"#
        )
    }

    fn audio(format_id: &str, ext: &str, codec: &str, bitrate: f64, url: &str) -> String {
        format!(
            r#"{{"format_id":"{format_id}","ext":"{ext}","vcodec":"none",
                "acodec":"{codec}","abr":{bitrate},"url":"{url}","protocol":"https"}}"#
        )
    }

    fn fixture(parts: &[String]) -> Metadata {
        metadata(&format!(
            r#"{{"title":"Fixture","formats":[{}]}}"#,
            parts.join(",")
        ))
    }

    #[test]
    fn prefers_a_muxed_mp4_at_the_same_height() {
        let meta = fixture(&[
            muxed("22", 1080, "https://example.com/22"),
            video("137", "mp4", "avc1.640028", 1080, "https://example.com/137"),
            audio("140", "m4a", "mp4a.40.2", 128.0, "https://example.com/140"),
        ]);

        let selection = select_streams(&meta, None).expect("selection");
        assert_eq!(selection.video.format_id, "22");
        assert!(selection.audio.is_none());
        assert!(!selection.needs_merge);
        assert_eq!(selection.ext, "mp4");
    }

    #[test]
    fn picks_avc1_video_and_original_m4a_for_dash() {
        let meta = fixture(&[
            muxed("22", 720, "https://example.com/22"),
            video("137", "mp4", "avc1.640028", 1080, "https://example.com/137"),
            video("248", "webm", "vp9", 1080, "https://example.com/248"),
            audio("140", "m4a", "mp4a.40.2", 128.0, "https://example.com/140"),
            audio("251", "webm", "opus", 160.0, "https://example.com/251"),
        ]);

        let selection = select_streams(&meta, None).expect("selection");
        assert_eq!(selection.video.format_id, "137");
        assert_eq!(selection.video.ext, "mp4");
        assert_eq!(selection.audio.expect("audio").format_id, "140");
        assert!(selection.needs_merge);
        assert_eq!(selection.ext, "mp4");
    }

    #[test]
    fn obeys_the_height_cap() {
        let meta = fixture(&[
            video("137", "mp4", "avc1.640028", 1080, "https://example.com/137"),
            video("136", "mp4", "avc1.64001F", 720, "https://example.com/136"),
            audio("140", "m4a", "mp4a.40.2", 128.0, "https://example.com/140"),
        ]);

        let uncapped = select_streams(&meta, None).expect("selection");
        assert_eq!(uncapped.video.height, Some(1080));

        let capped = select_streams(&meta, Some(720)).expect("selection");
        assert_eq!(capped.video.format_id, "136");
        assert_eq!(capped.video.height, Some(720));
    }

    #[test]
    fn rejects_hls_manifests() {
        let meta = metadata(
            r#"{"title":"HLS","formats":[
                {"format_id":"96","ext":"mp4","vcodec":"avc1","acodec":"mp4a",
                 "height":1080,"url":"https://example.com/master.m3u8",
                 "protocol":"m3u8_native"}
            ]}"#,
        );
        assert!(matches!(
            select_streams(&meta, None),
            Err(ResolveError::NoStream { kind: "video" })
        ));
    }

    #[test]
    fn skips_hls_when_a_direct_stream_exists() {
        let meta = fixture(&[
            muxed("18", 360, "https://example.com/18"),
            r#"{"format_id":"96","ext":"mp4","vcodec":"avc1","acodec":"mp4a",
                "height":1080,"url":"https://example.com/master.m3u8",
                "protocol":"m3u8_native"}"#
                .to_string(),
        ]);

        let selection = select_streams(&meta, None).expect("selection");
        assert_eq!(selection.video.format_id, "18");
        assert_eq!(selection.video.height, Some(360));
    }

    #[test]
    fn errors_without_any_video_track() {
        let meta = fixture(&[audio(
            "140",
            "m4a",
            "mp4a.40.2",
            128.0,
            "https://example.com/140",
        )]);
        assert!(matches!(
            select_streams(&meta, None),
            Err(ResolveError::NoStream { kind: "video" })
        ));
    }

    #[test]
    fn matches_the_audio_container_to_the_video() {
        let meta = fixture(&[
            video("248", "webm", "vp9", 1080, "https://example.com/248"),
            audio("140", "m4a", "mp4a.40.2", 128.0, "https://example.com/140"),
            audio("251", "webm", "opus", 160.0, "https://example.com/251"),
        ]);

        let selection = select_streams(&meta, None).expect("selection");
        assert_eq!(selection.video.format_id, "248");
        assert_eq!(selection.audio.expect("audio").format_id, "251");
        assert_eq!(selection.ext, "webm");
    }

    #[test]
    fn prefers_original_language_audio() {
        let meta = metadata(
            r#"{"title":"Dubbed","formats":[
                {"format_id":"137","ext":"mp4","vcodec":"avc1","acodec":"none",
                 "height":1080,"url":"https://example.com/137","protocol":"https"},
                {"format_id":"140-en","ext":"m4a","vcodec":"none","acodec":"mp4a",
                 "abr":128,"language":"en","url":"https://example.com/en","protocol":"https"},
                {"format_id":"140-ja","ext":"m4a","vcodec":"none","acodec":"mp4a",
                 "abr":128,"language":"ja","url":"https://example.com/ja","protocol":"https"}
            ]}"#,
        );

        let selection = select_streams(&meta, None).expect("selection");
        assert_eq!(selection.audio.expect("audio").format_id, "140-en");
    }

    #[test]
    fn reports_total_bytes_when_both_sizes_are_known() {
        let meta = metadata(
            r#"{"title":"Sizes","formats":[
                {"format_id":"137","ext":"mp4","vcodec":"avc1","acodec":"none",
                 "height":1080,"filesize":1000,"url":"https://example.com/137","protocol":"https"},
                {"format_id":"140","ext":"m4a","vcodec":"none","acodec":"mp4a",
                 "abr":128,"filesize":250,"url":"https://example.com/140","protocol":"https"}
            ]}"#,
        );

        let selection = select_streams(&meta, None).expect("selection");
        assert_eq!(selection.total_bytes, Some(1250));
    }
}
