//! Chrome native-messaging host for RapidGrab.
//!
//! The browser launches this binary with no arguments and speaks to it over
//! stdio using native-messaging framing (little-endian u32 length prefix +
//! UTF-8 JSON). For each capture frame it spawns `rapidgrab-popup.exe`
//! (next to itself) so the Download window floats over the browser even
//! when the main GUI window is minimized; the popup's result decides whether
//! the GUI is handed the capture. Each exchange is answered with a
//! structured `{ok, error}` acknowledgement; EOF (browser disconnect) ends it.

use std::process::{Command, Stdio};

use anyhow::Result;
use rg_bridge::HostAck;

fn popup_exe_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("rapidgrab-popup.exe");
    candidate.is_file().then_some(candidate)
}

/// Serializes confirm windows across host processes.
///
/// Chrome starts one host per capture, so a burst of downloads would otherwise
/// stack one "New download" window per capture. A named mutex keeps a single
/// RapidGrab window on screen at a time; the other hosts wait their turn.
#[cfg(windows)]
struct PopupLock(winapi::um::winnt::HANDLE);

#[cfg(windows)]
impl PopupLock {
    fn acquire() -> Option<Self> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::synchapi::{CreateMutexW, WaitForSingleObject};
        use winapi::um::winbase::{WAIT_ABANDONED, WAIT_OBJECT_0};

        let name: Vec<u16> = "Local\\RapidGrab-Popup\0".encode_utf16().collect();
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        // Abandoned means the previous popup process died without releasing;
        // the mutex is granted to us in that case too.
        let waited = unsafe { WaitForSingleObject(handle, 5 * 60 * 1000) };
        if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
            Some(Self(handle))
        } else {
            unsafe { CloseHandle(handle) };
            None
        }
    }
}

#[cfg(windows)]
impl Drop for PopupLock {
    fn drop(&mut self) {
        unsafe {
            winapi::um::synchapi::ReleaseMutex(self.0);
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
struct PopupLock;

#[cfg(not(windows))]
impl PopupLock {
    fn acquire() -> Option<Self> {
        Some(Self)
    }
}

fn run_popup(request: &rg_bridge::CaptureRequest) -> Result<rg_bridge::CaptureRequest> {
    let exe = popup_exe_path()
        .ok_or_else(|| anyhow::anyhow!("rapidgrab-popup.exe not found next to the host"))?;
    // Wait for the previous capture's window to close before showing ours.
    // The lock is held until this popup exits (dropped at the end of the fn).
    let _lock = PopupLock::acquire();
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().unwrap();
        rg_bridge::write_frame(&mut stdin, request)?;
    }
    let stdout = child.stdout.take().unwrap();
    let result = rg_bridge::read_frame(stdout);
    let status = child.wait()?;
    let _ = status;
    result.map(|r| {
        // Empty url is the popup's Cancel marker.
        r
    })
}

fn main() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    loop {
        let request = match rg_bridge::read_frame(&mut reader) {
            Ok(request) => request,
            Err(_) => break,
        };

        // Ask the popup first; only confirmed captures reach the GUI, so
        // Cancel leaves the browser's own download alone.
        let ack = match run_popup(&request) {
            Ok(confirm) if !confirm.url.is_empty() => match rg_bridge::send_capture(&confirm) {
                Ok(()) => HostAck {
                    ok: true,
                    error: None,
                },
                Err(e) => HostAck {
                    ok: false,
                    error: Some(format!("{e:#}")),
                },
            },
            // Popup missing (dev build without the popup binary): fall back
            // to forwarding directly so the GUI popup still works.
            Err(e) if popup_exe_path().is_none() => {
                let _ = e;
                match rg_bridge::send_capture(&request) {
                    Ok(()) => HostAck {
                        ok: true,
                        error: None,
                    },
                    Err(e) => HostAck {
                        ok: false,
                        error: Some(format!("{e:#}")),
                    },
                }
            }
            _ => HostAck {
                ok: false,
                error: Some("cancelled".to_string()),
            },
        };

        if rg_bridge::write_ack_frame(stdout.lock(), &ack).is_err() {
            break;
        }
    }
    Ok(())
}
