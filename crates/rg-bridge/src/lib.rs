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
    Ok(msg)
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
    let mut stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)
        .with_context(|| format!("GUI pipe {PIPE_NAME} not available"))?;
    write_frame(&mut stream, req)
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
                serde_json::from_slice::<CaptureRequest>(&body).ok()
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
            content_disposition_filename: Some("file.bin".into()),
            content_type: Some("application/octet-stream".into()),
            content_length: Some(123),
            source: Some("intercept".into()),
            auto_start: false,
            output: None,
            confirmed: false,
            dont_ask_again: false,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let back = read_frame(&buf[..]).unwrap();
        assert_eq!(back.url, req.url);
        assert_eq!(back.filename, req.filename);
        assert_eq!(back.referer, req.referer);
        assert_eq!(back.cookies, req.cookies);
        assert_eq!(back.user_agent, req.user_agent);
        assert_eq!(back.content_length, req.content_length);
        assert_eq!(back.source, req.source);
    }

    #[test]
    fn frame_rejects_empty() {
        let mut buf = Vec::new();
        (0u32).to_le_bytes().iter().for_each(|b| buf.push(*b));
        assert!(read_frame(&buf[..]).is_err());
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
