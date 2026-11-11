//! Browser-capture bridge between the extension and the GUI.
//!
//! The extension uses Chrome native messaging (4-byte little-endian length
//! prefix + UTF-8 JSON). This crate wraps that framing and the named pipe
//! `\\.\pipe\rapidgrab-capture` that the native host writes to and the GUI
//! reads from.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Pipe name used by the native host and the GUI (Windows).
pub const PIPE_NAME: &str = r"\\.\pipe\rapidgrab-capture";

/// True when the URL path names a media file (a direct download) rather than
/// a page that a resolver must handle.
///
/// Used by the capture popups: a `media`-sourced capture whose URL ends in a
/// media extension came from a player's stream URL (MSE players such as
/// Facebook expose no `currentSrc`), so it is probed and downloaded directly
/// instead of being labelled "Video page".
pub fn is_media_file_url(url: &str) -> bool {
    const MEDIA_EXTENSIONS: [&str; 11] = [
        ".mp4", ".m4v", ".webm", ".mkv", ".mov", ".avi", ".flv", ".mp3", ".m4a", ".wav", ".ogg",
    ];
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    MEDIA_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// One captured download from the browser.
/// Every field except `url` is optional; missing values are filled in by the GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureRequest {
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Raw page/video title (og:title, watch-page h1, post text, …). Cleaned
    /// by `rg_core::clean_title`; the extension deliberately sends it raw.
    #[serde(default)]
    pub title: Option<String>,
    /// Value of the `Content-Disposition: filename=` (or `filename*=`) header.
    #[serde(default)]
    pub content_disposition_filename: Option<String>,
    /// Value of the `Content-Type` response header.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Value of the `Content-Length` response header, when known.
    #[serde(default)]
    pub content_length: Option<u64>,
    /// How the capture was triggered: `click`, `link`, `intercept`, `page`.
    #[serde(default)]
    pub source: Option<String>,
    /// Signed URL of the sibling audio DASH rendition, when the player's
    /// MSE streams the video and audio separately (Facebook Reels). The
    /// downloader merges the two into the final file; `None` means the main
    /// URL is expected to carry audio on its own.
    #[serde(default)]
    pub audio_url: Option<String>,
    /// `Content-Type` reported for the audio rendition.
    #[serde(default)]
    pub audio_content_type: Option<String>,
    /// Full size of the audio rendition, when the CDN reported it.
    #[serde(default)]
    pub audio_content_length: Option<u64>,
    /// Cookies scoped to the audio host, when it differs from the video
    /// host's. Falls back to `cookies` when absent.
    #[serde(default)]
    pub audio_cookies: Option<String>,
    /// True when the capture is a video-only DASH rendition and no sibling
    /// audio track could be identified. The file is still downloaded (it may
    /// be a muxed URL); this only tells the UI to show a warning.
    #[serde(default)]
    pub audio_missing: bool,
    /// True when the extension was told to let the browser download continue.
    #[serde(default)]
    pub auto_start: bool,
    /// Save-as path chosen in the browser-side popup (confirmed captures).
    #[serde(default)]
    pub output: Option<String>,
    /// True when a confirmed popup (rapidgrab-popup) produced this capture;
    /// the GUI enqueues it directly without showing its own popup.
    #[serde(default)]
    pub confirmed: bool,
    /// "Don't ask again for this site" checked in the popup.
    #[serde(default)]
    pub dont_ask_again: bool,
}

/// Facebook's CDN encodes byte-range requests in the URL itself
/// (`bytestart`/`byteend`) for DASH/fMP4 playback. Downloading such a URL
/// returns a fragment of the file — big enough to look plausible, never
/// playable. Stripping the range parameters asks the CDN for the complete
/// object, which is what a download needs. Only applied to the Facebook
/// hosts that use these params.
pub fn normalize_capture_url(url: &str) -> String {
    let Some(host) = host_of(url) else {
        return url.to_string();
    };
    if !is_facebook_host(&host) {
        return url.to_string();
    }
    // url = base?query#fragment ; the fragment is not part of the request.
    let (without_fragment, fragment) = match url.find('#') {
        Some(h) => (&url[..h], &url[h..]),
        None => (url, ""),
    };
    let Some(q) = without_fragment.find('?') else {
        return url.to_string();
    };
    let base = &without_fragment[..q];
    let query = &without_fragment[q + 1..];
    let param_count = query.split('&').count();
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("");
            let lower = name.to_ascii_lowercase();
            lower != "bytestart" && lower != "byteend"
        })
        .collect();
    if kept.len() == param_count {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(fragment);
    out
}

/// Lower-case host (no userinfo/port/fragment) for `http(s)://` URLs.
fn host_of(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let host_start = scheme_end + 3;
    let rest = &url[host_start..];
    let end = rest
        .find(|c| ['/', '?', '#'].contains(&c))
        .unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn is_facebook_host(host: &str) -> bool {
    host == "facebook.com"
        || host.ends_with(".facebook.com")
        || host == "fbcdn.net"
        || host.ends_with(".fbcdn.net")
}

/// Normalize one captured frame's URLs (Facebook range params).
fn normalize_request(mut req: CaptureRequest) -> CaptureRequest {
    req.url = normalize_capture_url(&req.url);
    if let Some(audio_url) = req.audio_url.take() {
        req.audio_url = Some(normalize_capture_url(&audio_url));
    }
    req
}

/// Read one native-messaging frame: little-endian u32 length + JSON bytes.
pub fn read_frame<R: Read>(mut r: R) -> Result<CaptureRequest> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    let len = u32::from_le_bytes(buf) as usize;
    if len == 0 || len > 16 * 1024 * 1024 {
        anyhow::bail!("invalid frame length {len}");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let msg: CaptureRequest = serde_json::from_slice(&body)
        .with_context(|| format!("invalid capture JSON ({} bytes)", len))?;
    Ok(normalize_request(msg))
}

/// Write one native-messaging frame.
pub fn write_frame<W: Write>(mut w: W, req: &CaptureRequest) -> Result<()> {
    let body = serde_json::to_vec(req)?;
    let len = u32::try_from(body.len()).context("frame too large")?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Acknowledgement the native host sends back to the extension after
/// forwarding (or failing to forward) one capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAck {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Write one generic native-messaging frame (used for the host ack).
pub fn write_ack_frame<W: Write>(mut w: W, ack: &HostAck) -> Result<()> {
    let body = serde_json::to_vec(ack)?;
    let len = u32::try_from(body.len()).context("frame too large")?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Native host side: connect to the GUI pipe and send one request.
/// Returns an error if the GUI is not running.
pub fn send_capture(req: &CaptureRequest) -> Result<()> {
    let normalized = normalize_request(req.clone());
    let mut stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)
        .with_context(|| format!("GUI pipe {PIPE_NAME} not available"))?;
    write_frame(&mut stream, &normalized)
}

/// GUI side: bind the named pipe, spawn a listener thread, and return a guard
/// that keeps the pipe alive. The thread calls `on_request` for every frame.
/// Drop the guard to stop listening; the thread exits on the next accept error.
#[cfg(windows)]
pub fn listen<F>(on_request: F) -> Result<PipeGuard>
where
    F: Fn(CaptureRequest) + Send + Sync + 'static,
{
    use std::sync::Arc;
    let on_request = Arc::new(on_request);
    // ACL the pipe to the current user only (owner + SYSTEM). Without this,
    // any local process can connect and inject capture requests.
    let sd_text: Vec<u16> = "D:P(A;;GA;;;OW)(A;;GA;;;SY)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut sd: winapi::um::winnt::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut sd_size: u32 = 0;
    let converted = unsafe {
        winapi::shared::sddl::ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sd_text.as_ptr(),
            1,
            &mut sd,
            &mut sd_size,
        )
    };
    if converted == 0 || sd.is_null() {
        anyhow::bail!("ACL setup failed: {}", unsafe {
            winapi::um::errhandlingapi::GetLastError()
        });
    }
    let mut sa = winapi::um::minwinbase::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<winapi::um::minwinbase::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: winapi::shared::minwindef::FALSE,
    };

    let server = unsafe {
        winapi::um::namedpipeapi::CreateNamedPipeW(
            PIPE_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
                .as_ptr(),
            winapi::um::winbase::PIPE_ACCESS_DUPLEX,
            winapi::um::winbase::PIPE_TYPE_BYTE
                | winapi::um::winbase::PIPE_READMODE_BYTE
                | winapi::um::winbase::PIPE_WAIT,
            1,
            16 * 1024,
            16 * 1024,
            0,
            &mut sa,
        )
    };
    if server == -1isize as winapi::um::winnt::HANDLE {
        anyhow::bail!("CreateNamedPipe failed: {}", unsafe {
            winapi::um::errhandlingapi::GetLastError()
        });
    }
    // The pipe copied the SD at creation; free our copy.
    let _ = unsafe { winapi::um::winbase::LocalFree(sd as winapi::shared::minwindef::HLOCAL) };

    // HANDLE is a raw pointer and not Send; pass it as usize into the thread.
    let handle_usize = server as usize;
    let on_req = on_request.clone();
    std::thread::spawn(move || {
        let server = handle_usize as winapi::um::winnt::HANDLE;
        loop {
            // ConnectNamedPipe blocks until a client connects.
            let connected =
                unsafe { winapi::um::namedpipeapi::ConnectNamedPipe(server, std::ptr::null_mut()) };
            if connected == 0 {
                // ERROR_PIPE_CONNECTED (535) means client already connected.
                let err = unsafe { winapi::um::errhandlingapi::GetLastError() };
                if err != 535 {
                    // Transient errors (e.g. a client that vanished before
                    // connecting) must not kill the listener; only an invalid
                    // handle means the pipe is gone for good.
                    if err == 6 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
            }
            // Read one frame from this client.
            let req = unsafe {
                let mut buf = [0u8; 4];
                let mut read: u32 = 0;
                let ok = winapi::um::fileapi::ReadFile(
                    server,
                    buf.as_mut_ptr() as *mut _,
                    4,
                    &mut read,
                    std::ptr::null_mut(),
                );
                if ok == 0 || read != 4 {
                    winapi::um::namedpipeapi::DisconnectNamedPipe(server);
                    continue;
                }
                let len = u32::from_le_bytes(buf) as usize;
                if len == 0 || len > 16 * 1024 * 1024 {
                    winapi::um::namedpipeapi::DisconnectNamedPipe(server);
                    continue;
                }
                let mut body = vec![0u8; len];
                let ok = winapi::um::fileapi::ReadFile(
                    server,
                    body.as_mut_ptr() as *mut _,
                    len as u32,
                    &mut read,
                    std::ptr::null_mut(),
                );
                if ok == 0 {
                    winapi::um::namedpipeapi::DisconnectNamedPipe(server);
                    continue;
                }
                body.truncate(read as usize);
                serde_json::from_slice::<CaptureRequest>(&body)
                    .ok()
                    .map(normalize_request)
            };
            if let Some(req) = req {
                (on_req)(req);
            }
            unsafe { winapi::um::namedpipeapi::DisconnectNamedPipe(server) };
        }
    });

    Ok(PipeGuard { handle: server })
}

/// Guard that owns the named-pipe handle; closing it on drop disconnects the server.
#[cfg(windows)]
pub struct PipeGuard {
    handle: winapi::um::winnt::HANDLE,
}

#[cfg(windows)]
impl Drop for PipeGuard {
    fn drop(&mut self) {
        unsafe {
            winapi::um::namedpipeapi::DisconnectNamedPipe(self.handle);
            winapi::um::handleapi::CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
pub fn listen<F>(_on_request: F) -> Result<()>
where
    F: Fn(CaptureRequest) + Send + Sync + 'static,
{
    anyhow::bail!("named pipes are Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let req = CaptureRequest {
            url: "http://example.com/file.bin".into(),
            filename: Some("file.bin".into()),
            referer: Some("http://example.com/".into()),
            cookies: Some("a=1; b=2".into()),
            user_agent: Some("UA".into()),
            title: Some("My Video - YouTube".into()),
            content_disposition_filename: Some("file.bin".into()),
            content_type: Some("application/octet-stream".into()),
            content_length: Some(123),
            source: Some("intercept".into()),
            auto_start: false,
            output: None,
            confirmed: false,
            dont_ask_again: false,
            audio_url: None,
            audio_content_type: None,
            audio_content_length: None,
            audio_cookies: None,
            audio_missing: false,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let back = read_frame(&buf[..]).unwrap();
        assert_eq!(back.url, req.url);
        assert_eq!(back.filename, req.filename);
        assert_eq!(back.referer, req.referer);
        assert_eq!(back.cookies, req.cookies);
        assert_eq!(back.user_agent, req.user_agent);
        assert_eq!(back.title, req.title);
        assert_eq!(back.content_length, req.content_length);
        assert_eq!(back.source, req.source);
    }

    #[test]
    fn read_frame_normalizes_facebook_url() {
        let req = CaptureRequest {
            url: "https://video.fbcdn.net/v/abc.mp4?bytestart=100&byteend=200&oe=X".into(),
            filename: None,
            referer: None,
            cookies: None,
            user_agent: None,
            title: None,
            content_disposition_filename: None,
            content_type: Some("video/mp4".into()),
            content_length: Some(100),
            source: Some("media".into()),
            auto_start: false,
            output: None,
            confirmed: false,
            dont_ask_again: false,
            audio_url: Some(
                "https://video.fbcdn.net/v/abc_audio.mp4?bytestart=10&bytestart=11&oe=Y".into(),
            ),
            audio_content_type: Some("audio/mp4".into()),
            audio_content_length: Some(42),
            audio_cookies: Some("k=v".into()),
            audio_missing: true,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let back = read_frame(&buf[..]).unwrap();
        assert_eq!(back.url, "https://video.fbcdn.net/v/abc.mp4?oe=X");
        // The sibling DASH rendition gets the same range-param stripping.
        assert_eq!(
            back.audio_url.as_deref(),
            Some("https://video.fbcdn.net/v/abc_audio.mp4?oe=Y")
        );
        assert_eq!(back.audio_content_type.as_deref(), Some("audio/mp4"));
        assert_eq!(back.audio_content_length, Some(42));
        assert_eq!(back.audio_cookies.as_deref(), Some("k=v"));
        assert!(back.audio_missing);
    }

    #[test]
    fn frame_rejects_empty() {
        let mut buf = Vec::new();
        (0u32).to_le_bytes().iter().for_each(|b| buf.push(*b));
        assert!(read_frame(&buf[..]).is_err());
    }

    #[test]
    fn strips_facebook_range_params() {
        let n = normalize_capture_url(
            "https://scontent.fdca24-5.fna.fbcdn.net/v/t39.25447-2/123456789_n.mp4?_nc_cat=1&bytestart=0&byteend=158272&oh=abc&oe=DEF",
        );
        assert_eq!(
            n,
            "https://scontent.fdca24-5.fna.fbcdn.net/v/t39.25447-2/123456789_n.mp4?_nc_cat=1&oh=abc&oe=DEF"
        );
        assert!(!n.contains("bytestart") && !n.contains("byteend"));

        // Fragment is preserved.
        let n = normalize_capture_url(
            "https://video.fbcdn.net/v/abc.mp4?bytestart=1&oe=DEF#t=4",
        );
        assert_eq!(n, "https://video.fbcdn.net/v/abc.mp4?oe=DEF#t=4");

        // Only range params: drops the query entirely.
        let n = normalize_capture_url("https://video.fbcdn.net/v/abc.mp4?bytestart=1&byteend=2");
        assert_eq!(n, "https://video.fbcdn.net/v/abc.mp4");
    }

    #[test]
    fn leaves_other_hosts_alone() {
        // bytestart/byteend are real query params elsewhere; keep them.
        let n = normalize_capture_url("https://cdn.example.com/f.mp4?bytestart=1&byteend=2");
        assert_eq!(n, "https://cdn.example.com/f.mp4?bytestart=1&byteend=2");
        // Malformed / non-URL inputs pass through.
        assert_eq!(normalize_capture_url("not a url"), "not a url");
        assert_eq!(normalize_capture_url("https://video.fbcdn.net/v/abc.mp4"), "https://video.fbcdn.net/v/abc.mp4");
        // Uppercase param names still stripped.
        assert_eq!(
            normalize_capture_url("https://facebook.com/video.mp4?ByteStart=1&BYTEEND=2"),
            "https://facebook.com/video.mp4"
        );
    }

    #[test]
    fn media_file_url_detection() {
        // Facebook's sniffed stream URL: the extension is before the query.
        assert!(is_media_file_url(
            "https://scontent-fra3-2.xx.fbcdn.net/v/t42.1790-2/AQM123ZA.mp4?_nc_cat=101&_nc_oc=x"
        ));
        assert!(is_media_file_url("https://ex.com/a/Clip.WEBM"));
        assert!(is_media_file_url("https://ex.com/audio.mp3#t=10"));

        // Pages (including ones that merely mention a media extension).
        assert!(!is_media_file_url("https://www.facebook.com/"));
        assert!(!is_media_file_url("https://ex.com/watch?v=abc.mp4"));
        assert!(!is_media_file_url("https://ex.com/a.mp4/page"));
        assert!(!is_media_file_url(""));
    }
}
