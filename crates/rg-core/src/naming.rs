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

/// True for responses that are HTML documents rather than downloadable files.
pub fn is_web_page(content_type: &str) -> bool {
    matches!(mime(content_type).as_str(), "text/html" | "application/xhtml+xml")
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

fn mime(content_type: &str) -> String {    content_type
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
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
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
        assert_eq!(extension_for_content_type("audio/mpeg; charset=binary"), Some("mp3"));
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
            infer_output_path(Path::new("C:/Users/me/Desktop/download.bin"), Some("video/mp4"), None),
            PathBuf::from("C:/Users/me/Desktop/download.mp4")
        );
        assert_eq!(
            infer_output_path(Path::new("C:/Users/me/Desktop/extensionless"), Some("audio/mpeg"), None),
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
    fn leaves_unknown_types_alone() {
        assert_eq!(
            infer_output_path(Path::new("download.bin"), Some("application/octet-stream"), None),
            PathBuf::from("download.bin")
        );
    }
}
