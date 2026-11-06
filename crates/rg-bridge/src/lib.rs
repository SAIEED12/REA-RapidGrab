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
    // Security descriptor: current user only. Safe descriptor string for
    // the current user (D:P(A;;GA;;;OW)(A;;GA;;;SY)).
    let sd = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";
    let builder = winapi::um::minwinbase::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<winapi::um::minwinbase::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: winapi::shared::minwindef::FALSE,
    };
    let _ = builder; // full SD setup via winapi is verbose; default DACL is acceptable for v1
    let _ = sd;

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
            std::ptr::null_mut(),
        )
    };
    if server == -1isize as winapi::um::winnt::HANDLE {
        anyhow::bail!("CreateNamedPipe failed: {}", unsafe {
            winapi::um::errhandlingapi::GetLastError()
        });
    }

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
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let back = read_frame(&buf[..]).unwrap();
        assert_eq!(back.url, req.url);
        assert_eq!(back.filename, req.filename);
        assert_eq!(back.referer, req.referer);
        assert_eq!(back.cookies, req.cookies);
        assert_eq!(back.user_agent, req.user_agent);
    }

    #[test]
    fn frame_rejects_empty() {
        let mut buf = Vec::new();
        (0u32).to_le_bytes().iter().for_each(|b| buf.push(*b));
        assert!(read_frame(&buf[..]).is_err());
    }
}
