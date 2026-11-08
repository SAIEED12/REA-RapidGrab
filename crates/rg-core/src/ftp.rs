//! FTP support: one control connection plus one passive data connection.
//!
//! The server must advertise `EPSV` or `PASV`; active (`PORT`) mode is not
//! supported. Resume uses `SIZE` to learn the total and `REST <offset>`
//! before `RETR`, appending into the same `.rgtmp` file as the HTTP
//! single-stream fallback, then an atomic rename on success.
//!
//! Deliberate limitations (documented for the phase 3 gate):
//! * one data connection — parallel `REST` segments need a deeper
//!   server-capability probe and come later,
//! * no FTPS, and no proxying of the control/data sockets (HTTP CONNECT
//!   proxies do not carry raw TCP for us; SOCKS support would need a
//!   hand-written client for tokio).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{debug, info};
use tokio::sync::Mutex as AsyncMutex;

use crate::engine::{Progress, ProgressFn, Stats};
use crate::error::{Error, Result};
use crate::limiter::Limiter;
use crate::net::Credentials;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DATACONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_BUF: usize = 64 * 1024;

/// Reply code plus full reply text.
type Reply = (u16, String);

pub async fn download(
    url: &str,
    output: &Path,
    speed_limit: Option<u64>,
    credentials: Option<&Credentials>,
    cancel: Option<Arc<AtomicBool>>,
    on_progress: Option<ProgressFn>,
    started: Instant,
    resume: bool,
) -> Result<Stats> {
    if is_cancelled(cancel.as_ref()) {
        return Err(Error::Cancelled);
    }
    let parsed = parse_url(url)?;

    let mut control = FtpControl::connect((parsed.host.as_str(), parsed.port)).await?;
    login(&mut control, url, credentials).await?;

    let path = parsed.path.clone();
    command(&mut control, "TYPE I").await?;
    expect(&mut control, &[200, 250]).await?;

    // Size first: without it, resume and a total are impossible.
    command(&mut control, &format!("SIZE {path}")).await?;
    let (code, text) = last_reply(&mut control).await;
    let total: Option<u64> = if code == 213 {
        text.split_whitespace()
            .next_back()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|&size| size > 0)
    } else {
        None
    };

    // Resume: reuse an existing .rgtmp as the byte offset.
    let tmp = tmp_path(output);
    let mut existing = 0u64;
    if resume {
        if let Ok(meta) = tokio::fs::metadata(&tmp).await {
            existing = meta.len();
            if let Some(total) = total {
                if existing > total {
                    debug!("discarding .rgtmp longer than the remote file");
                    existing = 0;
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
            }
        }
    }

    if existing > 0 {
        command(&mut control, &format!("REST {existing}")).await?;
        let (code, _) = last_reply(&mut control).await;
        if code != 350 {
            // Server ignores REST: start over rather than corrupt the file.
            info!("FTP server does not honour REST; restarting from 0");
            existing = 0;
            let _ = tokio::fs::remove_file(&tmp).await;
        } else {
            info!("resuming FTP transfer at offset {existing}");
        }
    }

    // Passive side: try EPSV, fall back to PASV.
    command(&mut control, "EPSV").await?;
    let (code, text) = last_reply(&mut control).await;
    let data_port = if code == 229 {
        parse_epsv(&text)
    } else {
        command(&mut control, "PASV").await?;
        let (code, text) = last_reply(&mut control).await;
        if code != 227 {
            return Err(Error::Ftp(format!(
                "passive mode failed ({} {})",
                code, text
            )));
        }
        parse_pasv(&text)
    };
    let Some(data_port) = data_port else {
        return Err(Error::Ftp("passive reply was not parseable".into()));
    };

    let mut data = tokio::time::timeout(
        DATACONNECT_TIMEOUT,
        TcpStream::connect((parsed.host.as_str(), data_port)),
    )
    .await
    .map_err(|_| Error::Ftp("data connection timed out".into()))??;

    command(&mut control, &format!("RETR {path}")).await?;
    let (code, text) = last_reply(&mut control).await;
    if ![125, 150].contains(&code) {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ftp_error(&text));
    }

    let written = stream_data(
        &mut data,
        &tmp,
        existing,
        total,
        speed_limit,
        cancel,
        on_progress,
        output,
    )
    .await?;

    // The server confirms with 226; anything else after EOF is suspicious.
    let reply = control.read_reply().await.unwrap_or((0, String::new()));
    if ![226, 250].contains(&reply.0) {
        return Err(Error::Ftp(format!(
            "unexpected post-transfer reply {} {}",
            reply.0, reply.1
        )));
    }
    drop(data);
    // QUIT then close: the server may answer or hang up either way.
    let _ = command(&mut control, &format!("QUIT")).await;
    drop(control);
    tokio::fs::rename(&tmp, output).await?;
    info!("FTP download complete: {written} bytes");

    Ok(Stats {
        total: written,
        elapsed: started.elapsed(),
        segments: 1,
        ranged: false,
        output: output.to_path_buf(),
    })
}

fn is_cancelled(flag: Option<&Arc<AtomicBool>>) -> bool {
    flag.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn tmp_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push(".rgtmp");
    PathBuf::from(name)
}

fn ftp_error(reply: &str) -> Error {
    let code: Option<u16> = reply
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u16>().ok());
    match code {
        Some(530) => Error::Ftp(format!("login rejected: {reply}")),
        _ => Error::Ftp(format!("unexpected FTP reply: {reply}")),
    }
}

/// Data transfer loop: append to `tmp` starting at `existing` bytes.
///
/// Progress is emitted at most every 500 ms, mirroring the behaviour of the
/// engine's single-stream download.
async fn stream_data(
    data: &mut TcpStream,
    tmp: &Path,
    existing: u64,
    total: Option<u64>,
    speed_limit: Option<u64>,
    cancel: Option<Arc<AtomicBool>>,
    on_progress: Option<ProgressFn>,
    output: &Path,
) -> Result<u64> {
    let mut file = if existing > 0 {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(tmp)
            .await?
    } else {
        tokio::fs::File::create(tmp).await?
    };

    let limiter = speed_limit.map(Limiter::new);
    let mut buffer = vec![0u8; READ_BUF];
    let mut written: u64 = existing;
    let mut last_t = Instant::now();
    let mut last_bytes: u64 = 0;

    loop {
        if is_cancelled(cancel.as_ref()) {
            file.flush().await?;
            return Err(Error::Cancelled);
        }
        let n = data.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n]).await?;
        written += n as u64;
        if let Some(l) = &limiter {
            l.acquire(n as u64).await;
        }
        if last_t.elapsed() >= Duration::from_millis(500) {
            if let Some(cb) = &on_progress {
                let dt = last_t.elapsed().as_secs_f64().max(1e-9);
                cb(Progress {
                    downloaded: written,
                    total: total.unwrap_or(0),
                    speed_bps: written.saturating_sub(last_bytes) as f64 / dt,
                    connections: 1,
                    output: output.to_path_buf(),
                });
            }
            last_t = Instant::now();
            last_bytes = written;
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(written)
}

/// Parsed ftp:// URL, percent-decoded values.
struct FtpUrlInfo {
    host: String,
    port: u16,
    path: String,
    user: Option<String>,
    password: Option<String>,
}

fn parse_url(url: &str) -> Result<FtpUrlInfo> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::Ftp(format!("bad URL: {e}")))?;
    if parsed.scheme().eq_ignore_ascii_case("ftps") {
        return Err(Error::Ftp("ftps is not supported".into()));
    }
    if !parsed.scheme().eq_ignore_ascii_case("ftp") {
        return Err(Error::Ftp("URL is not ftp://".into()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Ftp("URL host missing".into()))?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(21);
    // Percent-decode; strip any ;type= suffix and trailing slashes.
    let path_raw = parsed.path();
    let path_full = decode_component(path_raw);
    let path = path_full
        .split(';')
        .next()
        .unwrap_or(&path_full)
        .trim_end_matches('/')
        .to_string();
    if path.is_empty() || !path.contains('/') {
        return Err(Error::Ftp("FTP URL needs a file path".into()));
    }
    let user_raw = parsed.username().to_string();
    let user = if user_raw.is_empty() {
        None
    } else {
        Some(decode_component(&user_raw)).filter(|user| !user.is_empty())
    };
    let password = parsed.password().map(decode_component);
    Ok(FtpUrlInfo {
        host,
        port,
        path,
        user,
        password,
    })
}

fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(hex) = value.get(i + 1..i + 3) {
                if let Ok(code) = u8::from_str_radix(hex, 16) {
                    out.push(code);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Control connection helper: one command, one reply at a time.
struct FtpControl {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: AsyncMutex<tokio::net::tcp::OwnedWriteHalf>,
    last: Option<Reply>,
}

impl FtpControl {
    async fn connect(addr: impl ToSocketAddrs) -> Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Ftp("control connection timed out".into()))??;
        let (read_half, write_half) = stream.into_split();
        let mut control = Self {
            reader: BufReader::with_capacity(4096, read_half),
            writer: AsyncMutex::new(write_half),
            last: None,
        };
        // Consume and validate the greeting banner ("220 ..."), which the
        // server sends unprompted. 4xx/5xx (e.g. "421 too many users") is a
        // refusal before any login attempt.
        let (code, text) = control.read_reply().await?;
        if code != 220 {
            return Err(Error::Ftp(format!("FTP server greeting {code}: {text}")));
        }
        control.last = None;
        Ok(control)
    }

    async fn read_reply(&mut self) -> Result<Reply> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(Error::Ftp("control connection closed".into()));
        }
        let trimmed = line.trim_end().to_string();
        // Multi-line: "ddd-text" ... last line "ddd ".
        if trimmed.len() >= 4 {
            let marker = &trimmed[..3];
            let separator = trimmed.as_bytes()[3];
            if separator == b'-' && marker.chars().all(|c| c.is_ascii_digit()) {
                let code: u16 = marker.parse().unwrap_or(0);
                let mut text = trimmed[4..].to_string();
                loop {
                    line.clear();
                    let n = self.reader.read_line(&mut line).await?;
                    if n == 0 {
                        return Ok((code, text));
                    }
                    let t = line.trim_end().to_string();
                    text.push('\n');
                    text.push_str(&t);
                    if t.len() >= 4 && t[..3] == *marker && t.as_bytes()[3] == b' ' {
                        break;
                    }
                }
                self.last = Some((code, text.clone()));
                return Ok((code, text));
            }
        }
        let code: u16 = trimmed[..3].parse().map_err(|_| {
            Error::Ftp(format!("unreadable FTP reply: {trimmed}"))
        })?;
        let text = trimmed
            .get(4..)
            .map(str::to_string)
            .unwrap_or_default();
        self.last = Some((code, text.clone()));
        Ok((code, text))
    }
}

/// Send a command; stores the reply but returns `()`.
async fn command(control: &mut FtpControl, line: &str) -> Result<()> {
    debug!("FTP > {line}");
    let mut writer = control.writer.lock().await;
    writer
        .write_all(line.as_bytes())
        .await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    drop(writer);
    control.read_reply().await?;
    Ok(())
}

/// Send a command and require one of `codes`.
async fn expect(control: &mut FtpControl, codes: &[u16]) -> Result<()> {
    let (code, text) = last_reply(control).await;
    if !codes.contains(&code) {
        return Err(ftp_error(&format!("{code} {text}")));
    }
    Ok(())
}

async fn login(control: &mut FtpControl, url: &str, credentials: Option<&Credentials>) -> Result<()> {
    let parsed = parse_url(url)?;
    // URL userinfo wins; otherwise fall back to the generic HTTP credentials
    // (the Advanced form / `--user`) so the GUI auth prompt can serve FTP too.
    let user = parsed
        .user
        .or_else(|| credentials.map(|credentials| credentials.username.clone()));
    let password = parsed
        .password
        .or_else(|| credentials.map(|credentials| credentials.password.clone()));

    let user = user.unwrap_or_else(|| "anonymous".to_string());
    let password = password.unwrap_or_else(|| "rapidgrab@local".to_string());

    command(control, &format!("USER {user}")).await?;
    let (code, text) = last_reply(control).await;
    match code {
        230 | 202 => Ok(()),
        331 => {
            command(control, &format!("PASS {password}")).await?;
            let (code, text) = last_reply(control).await;
            match code {
                230 | 202 => Ok(()),
                421 | 530 => Err(Error::FtpAuth(format!("login rejected: {text}"))),
                _ => Err(Error::Ftp(format!("unexpected login reply {code}: {text}"))),
            }
        }
        421 | 530 => Err(Error::FtpAuth(format!("login rejected: {text}"))),
        _ => Err(Error::Ftp(format!("unexpected login reply {code}: {text}"))),
    }
}

async fn last_reply(control: &mut FtpControl) -> Reply {
    control.last.clone().unwrap_or((0, String::new()))
}

/// `227 Passive Mode (h1,h2,h3,h4,p1,p2)`
fn parse_pasv(text: &str) -> Option<u16> {
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let inner = &text[open + 1..close];
    let nums: Vec<u16> = inner
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    if nums.len() < 6 {
        return None;
    }
    Some(nums[4] * 256 + nums[5])
}

/// `229 Entering Extended Passive Mode (|||PORT|)`
fn parse_epsv(text: &str) -> Option<u16> {
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let inner = &text[open + 1..close];
    let digits: String = inner.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
