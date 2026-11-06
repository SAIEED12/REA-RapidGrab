//! Chrome native-messaging host for RapidGrab.
//!
//! The browser launches this binary with no arguments and speaks to it over
//! stdio using native-messaging framing (little-endian u32 length prefix +
//! UTF-8 JSON). It reads exactly one frame, forwards it to the GUI named
//! pipe (`\\.\pipe\rapidgrab-capture`), writes one acknowledgement frame to
//! stdout, and exits. The GUI remains responsible for pre-filling the form
//! (confirm-first); nothing here auto-starts a download.

use anyhow::Result;
use rg_bridge::CaptureRequest;

fn main() -> Result<()> {
    let request = rg_bridge::read_frame(std::io::stdin())?;
    rg_bridge::send_capture(&request)?;
    // Native messaging expects a response frame.
    rg_bridge::write_frame(
        std::io::stdout(),
        &CaptureRequest {
            url: String::new(),
            filename: None,
            referer: None,
            cookies: None,
            user_agent: None,
        },
    )
}
