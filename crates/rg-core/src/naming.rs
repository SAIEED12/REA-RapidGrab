//! Filename and content-type helpers.
//!
//! Many download URLs carry no filename at all (`/watch?v=…`,
//! `/videoplayback?…`), so the GUI falls back to `download.bin`. When the
//! caller's path has no extension (or ends in `.bin`) we derive a better one
//! from `Content-Disposition` first and the response `Content-Type` second.
//!
//! HTML responses are treated as web pages, not files: saving YouTube's
//! `/watch` page as `.bin` is almost never what the user meant.

use std::path::{Path, PathBuf};

use unicode_segmentation::UnicodeSegmentation;

/// True for responses that are HTML documents rather than downloadable files.
pub fn is_web_page(content_type: &str) -> bool {
    matches!(
        mime(content_type).as_str(),
        "text/html" | "application/xhtml+xml"
    )
}

/// Map a MIME type to a file extension (without the dot).
pub fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    Some(match mime(content_type).as_str() {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/html" | "application/xhtml+xml" => "html",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => return None,
    })
}

/// Extract a filename from a `Content-Disposition` header value.
///
/// Handles both `filename="name.ext"` and RFC 5987
/// `filename*=UTF-8''percent-encoded`. Path components are stripped so a
/// hostile header cannot escape the output directory.
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;

    for part in value.split(';').skip(1) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let raw = raw.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename" => plain = safe_basename(raw),
            "filename*" => {
                // RFC 5987: charset'language'percent-encoded-name
                let encoded = match raw.splitn(3, '\'').nth(2) {
                    Some(encoded) => encoded,
                    None => raw,
                };
                let decoded = percent_decode(encoded).unwrap_or_else(|| encoded.to_string());
                extended = safe_basename(&decoded);
            }
            _ => {}
        }
    }

    extended.or(plain)
}

/// Choose the path a download should actually be written to.
///
/// The caller's path wins when it already has a meaningful extension. A path
/// with no extension, or the GUI's `download.bin` placeholder, is refined from
/// `Content-Disposition`, then `Content-Type`; otherwise it is left untouched.
pub fn infer_output_path(
    output: &Path,
    content_type: Option<&str>,
    content_disposition: Option<&str>,
) -> PathBuf {
    let is_placeholder = match output.extension().and_then(|e| e.to_str()) {
        None => true,
        Some(ext) => ext.is_empty() || ext.eq_ignore_ascii_case("bin"),
    };
    if !is_placeholder {
        return output.to_path_buf();
    }

    let parent = output.parent().unwrap_or_else(|| Path::new(""));
    let type_ext = content_type.and_then(extension_for_content_type);

    if let Some(name) = content_disposition.and_then(filename_from_content_disposition) {
        let mut candidate = parent.join(name);
        if candidate.extension().is_none() {
            if let Some(ext) = type_ext {
                candidate.set_extension(ext);
            }
        }
        return candidate;
    }

    match type_ext {
        Some(ext) => {
            let mut path = output.to_path_buf();
            path.set_extension(ext);
            path
        }
        None => output.to_path_buf(),
    }
}

/// Turn an arbitrary title into a safe single-component filename.
///
/// Unlike `Content-Disposition` names (where a path must be stripped), a title
/// such as `AC/DC Live` should keep its words: invalid characters become
/// spaces, runs of whitespace collapse, and long titles are truncated.
/// Returns `None` when nothing usable remains.
pub fn sanitize_filename(name: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(120).collect();
    let trimmed = truncated.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Exact, case-insensitive labels that name nothing useful.
///
/// These are the site shells and player placeholders YouTube/Facebook expose
/// through `document.title`, `og:title`, and video elements. Free-form titles
/// such as `"Watch our new tour"` must still pass.
pub fn is_generic_title(title: &str) -> bool {
    let name = title.trim().to_lowercase();
    matches!(
        name.as_str(),
        "youtube video player"
            | "video player"
            | "video"
            | "videos"
            | "watch"
            | "player"
            | "untitled"
            | "facebook"
            | "youtube"
            | "youtube music"
            | "home"
            | "feed"
            | "reels"
            | "reel viewer"
            | "reels viewer"
            | "videoplayback"
            | "download"
            | "file"
            | "index"
            | "master"
            | "playlist"
    )
}

const SITE_SUFFIXES: &[&str] = &[
    "YouTube Music",
    "YouTube",
    "Facebook",
    "Instagram",
    "TikTok",
    "Twitter",
    "Vimeo",
    "Dailymotion",
    "SoundCloud",
    "Reddit",
];

/// Strip a trailing ` - <site>` / ` | <site>` / ` / <site>` suffix.
fn strip_site_suffix(text: &str) -> &str {
    let trimmed = text.trim_end();
    for site in SITE_SUFFIXES {
        for sep in [" - ", " – ", " — ", " | ", " / "] {
            let suffix = format!("{sep}{site}");
            let n = suffix.chars().count();
            let Some((start, _)) = trimmed.char_indices().rev().nth(n - 1) else {
                continue;
            };
            if trimmed[start..].eq_ignore_ascii_case(&suffix) {
                return trimmed[..start].trim_end();
            }
        }
    }
    trimmed
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Prefix of `text` containing at most `max` grapheme clusters.
fn grapheme_prefix(text: &str, max: usize) -> &str {
    if max == 0 {
        return "";
    }
    match text.grapheme_indices(true).nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Truncate on a grapheme (and preferably word) boundary so Bangla conjuncts
/// and other multi-codepoint scripts are never cut in half.
fn truncate_at_boundary(text: &str, max: usize) -> String {
    if text.graphemes(true).count() <= max {
        return text.to_string();
    }
    let prefix = grapheme_prefix(text, max);
    if let Some(index) = prefix.rfind(|c: char| c.is_whitespace() || c == '-') {
        if index > prefix.len() / 2 {
            let cut = prefix[..index].trim_end();
            if !cut.is_empty() {
                return cut.to_string();
            }
        }
    }
    prefix.trim_end().to_string()
}

/// Turn a raw page/player title into a useful file-name stem.
///
/// Cleans the notification counter, a trailing ` - <site>` suffix, multi-line
/// captions, and generic placeholders. All title cleaning in the product goes
/// through this function: the extension sends raw titles on purpose.
pub fn clean_title(raw: &str) -> Option<String> {
    let mut text = raw.trim();
    // YouTube prepends an unread-notification counter: "(13) Video title".
    if let Some(rest) = text.strip_prefix('(') {
        if let Some(close) = rest.find(')') {
            let counter = &rest[..close];
            if !counter.is_empty() && counter.chars().all(|c| c.is_ascii_digit()) {
                text = rest[close + 1..].trim_start();
            }
        }
    }
    // A caption can span several lines; a title is its first line only.
    let text = text.lines().next().unwrap_or("").trim();
    let text = strip_site_suffix(text);
    let text = text.replace(" | ", " - ").replace('|', "-");
    let text = collapse_whitespace(&text);
    let text = truncate_at_boundary(&text, 300);
    if text.is_empty() || is_generic_title(&text) {
        None
    } else {
        Some(text)
    }
}

/// Maximum grapheme count for an editable file-name stem. Leaves room for the
/// extension and deep folders under the Windows 260-character path limit.
pub const MAX_FILE_STEM_GRAPHEMES: usize = 100;

fn is_reserved_stem(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or("").trim();
    let lower = base.to_ascii_lowercase();
    matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || (lower.len() == 4
            && (lower.starts_with("com") || lower.starts_with("lpt"))
            && lower.as_bytes()[3].is_ascii_digit())
}

/// Turn a title into a Windows-safe stem (no extension).
///
/// Invalid characters become `-`, whitespace and separators collapse, trailing
/// dots/spaces are removed, reserved device names get a trailing `_`, and the
/// result is capped on a grapheme/word boundary. Unicode is preserved.
pub fn sanitize_file_stem(stem: &str) -> String {
    let mut cleaned = String::with_capacity(stem.len());
    let mut last_space = false;
    let mut skip_space = false;
    for ch in stem.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            // Replace the forbidden character, absorbing the spaces that
            // usually surround it ("AC/DC: Live" → "AC-DC-Live") while
            // leaving genuine hyphens like " - " untouched.
            while cleaned.ends_with(' ') {
                cleaned.pop();
            }
            if !cleaned.ends_with('-') {
                cleaned.push('-');
            }
            last_space = false;
            skip_space = true;
        } else if ch.is_whitespace() {
            if skip_space {
                skip_space = false;
                continue;
            }
            if !last_space && !cleaned.is_empty() {
                cleaned.push(' ');
            }
            last_space = true;
        } else {
            cleaned.push(ch);
            last_space = false;
            skip_space = false;
        }
    }

    let mut out = truncate_at_boundary(cleaned.trim(), MAX_FILE_STEM_GRAPHEMES);
    out = out
        .trim()
        .trim_end_matches(|c: char| c == '.' || c == ' ' || c == '-')
        .trim()
        .to_string();
    if out.is_empty() {
        return "download".to_string();
    }
    if is_reserved_stem(&out) {
        out.push('_');
    }
    out
}

/// Extensions we treat as real file names inside a title or URL.
const KNOWN_EXTENSIONS: &[&str] = &[
    "mp4", "webm", "mkv", "mov", "avi", "flv", "m4v", "mpeg", "mpg", "3gp", "mp3", "m4a",
    "aac", "ogg", "opus", "wav", "flac", "wma", "jpg", "jpeg", "png", "gif", "webp", "svg",
    "bmp", "tif", "tiff", "pdf", "zip", "rar", "7z", "tar", "gz", "bin", "doc", "docx",
    "xls", "xlsx", "ppt", "pptx", "txt", "csv", "json", "xml", "apk", "exe", "msi", "iso",
];

fn is_known_extension(ext: &str) -> bool {
    KNOWN_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(ext))
}

/// Split `name` into (stem, ".ext") only when the suffix is a known file type.
fn split_known_extension(name: &str) -> (String, String) {
    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot + 1..];
        if dot > 0
            && !ext.is_empty()
            && ext.len() <= 7
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
            && is_known_extension(ext)
        {
            return (
                name[..dot].to_string(),
                format!(".{}", ext.to_ascii_lowercase()),
            );
        }
    }
    (name.to_string(), String::new())
}

fn url_without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

fn host_of(url: &str) -> &str {
    let s = url_without_query(url).trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split('/').next().unwrap_or("");
    host.split(':').next().unwrap_or("")
}

fn site_label(host: &str) -> String {
    let host = host.trim().trim_start_matches("www.");
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    let pick = if labels.len() >= 2 {
        labels[labels.len() - 2]
    } else {
        host
    };
    pick.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn url_basename(url: &str) -> Option<String> {
    let path = url_without_query(url);
    let segment = path.rsplit('/').find(|part| !part.is_empty())?;
    let decoded = percent_decode(segment).unwrap_or_else(|| segment.to_string());
    (!decoded.is_empty()).then_some(decoded)
}

fn is_hash_like(stem: &str) -> bool {
    let len = stem.chars().count();
    if len < 16 || stem.contains(' ') || stem.contains('-') || stem.contains('_') {
        return false;
    }
    let lower = stem.chars().filter(|c| c.is_ascii_lowercase()).count();
    let upper = stem.chars().filter(|c| c.is_ascii_uppercase()).count();
    let digits = stem.chars().filter(|c| c.is_ascii_digit()).count();
    lower >= 3 && upper >= 3 && digits >= 3
}

fn extension_from_url(url: &str) -> Option<String> {
    let name = url_basename(url)?;
    let (_, ext) = split_known_extension(&name);
    (!ext.is_empty()).then_some(ext)
}

fn is_audio_type(content_type: &str) -> bool {
    mime(content_type).starts_with("audio/")
}

fn choose_extension(url: &str, content_type: Option<&str>, media_capture: bool) -> String {
    let url_ext = extension_from_url(url);
    if let Some(ext) = content_type.and_then(extension_for_content_type) {
        // A DASH audio track is often served from a *.mp4 URL. Never name it
        // as a video file.
        if content_type.is_some_and(is_audio_type) {
            return format!(".{ext}");
        }
        if let Some(ext) = url_ext {
            return ext;
        }
        return format!(".{ext}");
    }
    if let Some(ext) = url_ext {
        return ext;
    }
    if media_capture {
        return ".mp4".to_string();
    }
    ".bin".to_string()
}

fn kind_word(url: &str, content_type: Option<&str>) -> &'static str {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.starts_with("video/") {
        return "video";
    }
    if ct.starts_with("audio/") {
        return "audio";
    }
    if ct.starts_with("image/") {
        return "image";
    }
    let ext = url_basename(url)
        .and_then(|name| {
            name.rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_lowercase())
        })
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "flv" | "m4v" | "mpeg" | "mpg" | "3gp" => {
            "video"
        }
        "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav" | "flac" | "wma" => "audio",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" => "image",
        _ => "file",
    }
}

/// Split a name on its last short extension, regardless of type.
fn split_short_extension(name: &str) -> (&str, &str) {
    if let Some(dot) = name.rfind('.') {
        if dot > 0 && name.len() < dot + 8 {
            return (&name[..dot], &name[dot..]);
        }
    }
    (name, "")
}

/// Suggest a non-colliding name in `dir`: `name (1).ext`, `name (2).ext`, …
///
/// Only the suggestion is changed; an existing file is never overwritten by
/// the caller unless the user explicitly edits the field.
pub fn unique_file_name(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = split_short_extension(name);
    for n in 1..=9999 {
        let candidate = format!("{stem} ({n}){ext}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string()
}

/// Inputs for [`default_capture_name`].
#[derive(Debug, Clone, Copy)]
pub struct NamingContext<'a> {
    pub title: Option<&'a str>,
    pub content_disposition: Option<&'a str>,
    pub filename: Option<&'a str>,
    pub url: &'a str,
    pub referer: Option<&'a str>,
    pub content_type: Option<&'a str>,
    /// True when the browser's player button (not an intercepted file) made
    /// the capture; a media capture with no type/extension defaults to `.mp4`.
    pub media_capture: bool,
}

/// Resolve the default Save-as name for a browser capture.
///
/// Priority: authoritative `Content-Disposition` name → cleaned page/video
/// title → browser filename guess → readable URL basename → a dated
/// `<site>_<kind>` fallback. Generic values at any stage are skipped.
pub fn default_capture_name(ctx: &NamingContext<'_>) -> String {
    // 1. Content-Disposition names a real file; keep it unless generic.
    if let Some(name) = ctx
        .content_disposition
        .and_then(filename_from_content_disposition)
    {
        let (stem, ext) = split_known_extension(&name);
        if !stem.trim().is_empty() && !is_generic_title(&stem) {
            return format!("{}{}", sanitize_file_stem(&stem), ext);
        }
    }

    // 2. The page/video title. The extension sends it raw; clean_title owns
    // every normalization and generic check.
    if let Some(title) = ctx.title.and_then(clean_title) {
        let (stem, ext) = split_known_extension(&title);
        let ext = if ext.is_empty() {
            choose_extension(ctx.url, ctx.content_type, ctx.media_capture)
        } else {
            ext
        };
        return format!("{}{}", sanitize_file_stem(&stem), ext);
    }

    // 3. The extension's filename guess, when it is readable.
    if let Some(name) = ctx.filename.filter(|s| !s.trim().is_empty()) {
        let (stem, ext) = split_known_extension(name);
        if !stem.is_empty() && !is_generic_title(&stem) && !is_hash_like(&stem) {
            let ext = if ext.is_empty() {
                choose_extension(ctx.url, ctx.content_type, ctx.media_capture)
            } else {
                ext
            };
            return format!("{}{}", sanitize_file_stem(&stem), ext);
        }
    }

    // 4. A readable URL basename.
    if let Some(name) = url_basename(ctx.url) {
        let (stem, ext) = split_known_extension(&name);
        if !ext.is_empty() && !is_generic_title(&stem) && !is_hash_like(&stem) {
            return format!("{}{}", sanitize_file_stem(&stem), ext);
        }
    }

    // 5. Readable fallback: <site>_<kind>_<date>.<ext>.
    let host = ctx
        .referer
        .map(host_of)
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| host_of(ctx.url));
    let site = site_label(host);
    let site = if site.is_empty() {
        "download".to_string()
    } else {
        site
    };
    let kind = kind_word(ctx.url, ctx.content_type);
    let ext = choose_extension(ctx.url, ctx.content_type, ctx.media_capture);
    let date = chrono::Local::now().format("%Y-%m-%d");
    format!("{site}_{kind}_{date}{ext}")
}

fn mime(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn safe_basename(name: &str) -> Option<String> {
    let name = name.trim().trim_matches('"');
    if name.is_empty() {
        return None;
    }
    let base = Path::new(name).file_name()?.to_string_lossy();
    if base == "." || base == ".." {
        return None;
    }
    let cleaned: String = base
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_html_pages() {
        assert!(is_web_page("text/html; charset=utf-8"));
        assert!(is_web_page("application/xhtml+xml"));
        assert!(!is_web_page("video/mp4"));
    }

    #[test]
    fn maps_common_content_types() {
        assert_eq!(extension_for_content_type("video/mp4"), Some("mp4"));
        assert_eq!(
            extension_for_content_type("audio/mpeg; charset=binary"),
            Some("mp3")
        );
        assert_eq!(extension_for_content_type("application/pdf"), Some("pdf"));
        assert_eq!(extension_for_content_type("application/octet-stream"), None);
    }

    #[test]
    fn parses_content_disposition_filenames() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"clip.mp4\""),
            Some("clip.mp4".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename*=UTF-8''na%C3%AFve.mp4"),
            Some("naïve.mp4".to_string())
        );
        // Path components must never escape the output directory.
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"../../evil.mp4\""),
            Some("evil.mp4".to_string())
        );
        assert_eq!(filename_from_content_disposition("inline"), None);
    }

    #[test]
    fn keeps_a_real_extension() {
        assert_eq!(
            infer_output_path(
                Path::new("downloads/movie.mkv"),
                Some("video/mp4"),
                Some("attachment; filename=\"other.mp4\"")
            ),
            PathBuf::from("downloads/movie.mkv")
        );
    }

    #[test]
    fn replaces_the_bin_placeholder_from_content_type() {
        assert_eq!(
            infer_output_path(
                Path::new("C:/Users/me/Desktop/download.bin"),
                Some("video/mp4"),
                None
            ),
            PathBuf::from("C:/Users/me/Desktop/download.mp4")
        );
        assert_eq!(
            infer_output_path(
                Path::new("C:/Users/me/Desktop/extensionless"),
                Some("audio/mpeg"),
                None
            ),
            PathBuf::from("C:/Users/me/Desktop/extensionless.mp3")
        );
    }

    #[test]
    fn prefers_the_content_disposition_name() {
        assert_eq!(
            infer_output_path(
                Path::new("C:/Users/me/Desktop/download.bin"),
                Some("video/mp4"),
                Some("attachment; filename=\"Walk Through Text.mp4\"")
            ),
            PathBuf::from("C:/Users/me/Desktop/Walk Through Text.mp4")
        );
    }

    #[test]
    fn chooses_audio_extension_for_audio_tracks() {
        // A DASH audio track is often served from a .mp4 URL: the name must
        // not claim a video file.
        let name = default_capture_name(&NamingContext {
            title: Some("Cool reel post"),
            content_disposition: None,
            filename: None,
            url: "https://video.fbcdn.net/v/t39.25447-2/abc_n.mp4?oe=DFE",
            referer: None,
            content_type: Some("audio/mp4"),
            media_capture: true,
        });
        assert_eq!(name, "Cool reel post.m4a");
    }

    #[test]
    fn keeps_video_extension_for_mp4_tracks() {
        let name = default_capture_name(&NamingContext {
            title: Some("Cool reel post"),
            content_disposition: None,
            filename: None,
            url: "https://video.fbcdn.net/v/t39.25447-2/abc_n.mp4?oe=DFE",
            referer: None,
            content_type: Some("video/mp4"),
            media_capture: true,
        });
        assert_eq!(name, "Cool reel post.mp4");
    }

    #[test]
    fn leaves_unknown_types_alone() {
        assert_eq!(
            infer_output_path(
                Path::new("download.bin"),
                Some("application/octet-stream"),
                None
            ),
            PathBuf::from("download.bin")
        );
    }

    #[test]
    fn cleans_a_youtube_tab_title() {
        assert_eq!(
            clean_title(
                "(13) Learn NestJS Zero to Advanced | Complete NestJS Crash Course in Bangla - YouTube"
            )
            .as_deref(),
            Some("Learn NestJS Zero to Advanced - Complete NestJS Crash Course in Bangla")
        );
    }

    #[test]
    fn rejects_generic_titles() {
        for title in [
            "YouTube Video Player",
            "Video Player",
            "video",
            "watch",
            "player",
            "Untitled",
            "Facebook",
            "Reels viewer",
            "Reel viewer",
            "videoplayback",
            "download",
            "file",
            "index",
            "master",
            "playlist",
        ] {
            assert_eq!(clean_title(title), None, "{title:?} must be rejected");
        }
    }

    #[test]
    fn takes_the_first_caption_line() {
        assert_eq!(
            clean_title("  A great clip\nwatch the full video here  ").as_deref(),
            Some("A great clip")
        );
    }

    #[test]
    fn sanitizes_windows_file_stems() {
        assert_eq!(sanitize_file_stem("AC/DC: Live?"), "AC-DC-Live");
        assert_eq!(sanitize_file_stem("name. . "), "name");
        assert_eq!(sanitize_file_stem("a\\b:c*d?e\"f<g>h|i"), "a-b-c-d-e-f-g-h-i");
        assert_eq!(sanitize_file_stem("CON"), "CON_");
        assert_eq!(sanitize_file_stem("com1"), "com1_");
        assert_eq!(sanitize_file_stem("বাংলা ভিডিও"), "বাংলা ভিডিও");
        assert_eq!(sanitize_file_stem("   "), "download");
    }

    #[test]
    fn caps_long_stems_on_a_boundary() {
        let long = "বাংলা ".repeat(40);
        let stem = sanitize_file_stem(&long);
        assert!(stem.graphemes(true).count() <= MAX_FILE_STEM_GRAPHEMES);
        assert!(stem.ends_with("বাংলা"));
        // A single long word is cut on a grapheme boundary, never mid-conjunct.
        let word = sanitize_file_stem(&"বাংলা".repeat(40));
        assert!(word.graphemes(true).count() <= MAX_FILE_STEM_GRAPHEMES);
    }

    #[test]
    fn prefers_the_real_title_over_the_player_label() {
        let name = default_capture_name(&NamingContext {
            title: Some(
                "(13) Learn NestJS Zero to Advanced | Complete NestJS Crash Course in Bangla - YouTube",
            ),
            content_disposition: None,
            filename: Some("YouTube Video Player.mp4"),
            url: "https://www.youtube.com/watch?v=abc123",
            referer: Some("https://www.youtube.com/watch?v=abc123"),
            content_type: None,
            media_capture: true,
        });
        assert_eq!(
            name,
            "Learn NestJS Zero to Advanced - Complete NestJS Crash Course in Bangla.mp4"
        );
    }

    #[test]
    fn content_disposition_wins_unless_generic() {
        let real = default_capture_name(&NamingContext {
            title: Some("Nice page title"),
            content_disposition: Some("attachment; filename=\"report.pdf\""),
            filename: Some("report.pdf"),
            url: "https://example.com/download/xyz",
            referer: Some("https://example.com/article"),
            content_type: Some("application/pdf"),
            media_capture: false,
        });
        assert_eq!(real, "report.pdf");

        let generic = default_capture_name(&NamingContext {
            title: Some("Real video title"),
            content_disposition: Some("attachment; filename=\"download.bin\""),
            filename: Some("download.bin"),
            url: "https://example.com/watch",
            referer: Some("https://example.com/watch"),
            content_type: None,
            media_capture: true,
        });
        assert_eq!(generic, "Real video title.mp4");
    }

    #[test]
    fn suggests_a_numbered_name_when_the_file_exists() {
        let dir = std::env::temp_dir().join(format!("rg-naming-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Clip.mp4"), b"x").unwrap();
        assert_eq!(unique_file_name(&dir, "Clip.mp4"), "Clip (1).mp4");
        std::fs::write(dir.join("Clip (1).mp4"), b"x").unwrap();
        assert_eq!(unique_file_name(&dir, "Clip.mp4"), "Clip (2).mp4");
        assert_eq!(unique_file_name(&dir, "Other.mp4"), "Other.mp4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_a_readable_site_name() {
        let name = default_capture_name(&NamingContext {
            title: None,
            content_disposition: None,
            filename: Some("AQM6GyYVlQ5c123ABC.mp4"),
            url: "https://scontent-fra3-2.xx.fbcdn.net/v/t42.1790-2/AQM6GyYVlQ5c123ABC.mp4?_nc_cat=1",
            referer: Some("https://www.facebook.com/reel/123456"),
            content_type: Some("video/mp4"),
            media_capture: true,
        });
        assert!(name.starts_with("facebook_video_"));
        assert!(name.ends_with(".mp4"));
    }
}
