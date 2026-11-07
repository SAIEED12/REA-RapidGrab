//! Chrome native-messaging host for RapidGrab.
//!
//! The browser launches this binary with no arguments and speaks to it over
//! stdio using native-messaging framing (little-endian u32 length prefix +
//! UTF-8 JSON). It stays alive for the whole session: each frame is forwarded
//! to the GUI named pipe (`\\.\pipe\rapidgrab-capture`) and answered with a
//! structured `{ok, error}` acknowledgement; EOF (browser disconnect) ends it.
//! The GUI remains responsible for pre-filling the form (confirm-first);
//! nothing here auto-starts a download.

use anyhow::Result;
use rg_bridge::HostAck;

fn main() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    // Native messaging is stdin/stdout only: a broken stdin (EOF) means the
    // browser went away — exit cleanly, do not spin.
    let mut reader = stdin.lock();
    loop {
        let request = match rg_bridge::read_frame(&mut reader) {
            Ok(request) => request,
            Err(_) => break,
        };
        let ack = match rg_bridge::send_capture(&request) {
            Ok(()) => HostAck {
                ok: true,
                error: None,
            },
            Err(e) => HostAck {
                ok: false,
                error: Some(format!("{e:#}")),
            },
        };
        if rg_bridge::write_ack_frame(stdout.lock(), &ack).is_err() {
            // Browser closed the pipe mid-session; nothing left to do.
            break;
        }
    }
    Ok(())
}
