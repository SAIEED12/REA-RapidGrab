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

fn run_popup(request: &rg_bridge::CaptureRequest) -> Result<rg_bridge::CaptureRequest> {
    let exe = popup_exe_path()
        .ok_or_else(|| anyhow::anyhow!("rapidgrab-popup.exe not found next to the host"))?;
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
