//! FTP: in-process server covering anonymous and credentialed login (a wrong
//! password maps to `Error::FtpAuth`), EPSV with a PASV fallback, and
//! `REST`-based resume. One control connection plus one passive data
//! connection per transfer, exactly what `rg_core::ftp` uses.

use rg_core::{Credentials, Download, Error};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const FILE_NAME: &str = "file.bin";
const TOTAL: u64 = 512 * 1024;
const USER: &str = "alfred";
const PASS: &str = "s3cret";

fn body_byte(offset: u64) -> u8 {
    (offset % 251) as u8
}

#[derive(Clone, Copy)]
struct Config {
    /// Accept anonymous logins (`anonymous` with any password).
    anonymous_ok: bool,
    /// Reject `EPSV` with 500 to force the client onto PASV.
    epsv: bool,
}

impl Config {
    fn anonymous() -> Self {
        Self {
            anonymous_ok: true,
            epsv: true,
        }
    }
    fn auth_only() -> Self {
        Self {
            anonymous_ok: false,
            epsv: true,
        }
    }
    fn pasv_fallback() -> Self {
        Self {
            anonymous_ok: true,
            epsv: false,
        }
    }
}

async fn reply(tx: &mut tokio::net::tcp::OwnedWriteHalf, line: String) {
    let _ = tx.write_all(line.as_bytes()).await;
    let _ = tx.write_all(b"\r\n").await;
    let _ = tx.flush().await;
}

/// Handle one control connection to completion.
async fn handle_control(sock: TcpStream, config: Config) {
    let (rx, mut tx) = sock.into_split();
    let mut reader = BufReader::new(rx);

    reply(&mut tx, "220 RapidGrab test FTP ready".into()).await;

    // ---- login ----
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let request = line.trim_end().to_string();
    if !request.to_ascii_uppercase().starts_with("USER ") {
        reply(&mut tx, "502 expected USER".into()).await;
        return;
    }
    let user = request[5..].trim().to_string();

    if user == USER {
        reply(&mut tx, "331 password required".into()).await;
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let pass_ok = line.strip_prefix("PASS ").map(|p| p.trim_end() == PASS) == Some(true);
        if pass_ok {
            reply(&mut tx, "230 logged in".into()).await;
        } else {
            reply(&mut tx, "530 login incorrect".into()).await;
            return;
        }
    } else if config.anonymous_ok {
        reply(&mut tx, "230 anonymous login accepted".into()).await;
    } else {
        reply(&mut tx, "530 login incorrect".into()).await;
        return;
    }

    // ---- command loop ----
    let mut rest_offset: u64 = 0;
    let mut data_listener: Option<TcpListener> = None;

    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let command = line.trim_end().to_string();
        if command.is_empty() {
            continue;
        }
        let (verb, arg) = command.split_once(' ').unwrap_or((command.as_str(), ""));
        match verb.to_ascii_uppercase().as_str() {
            "TYPE" => reply(&mut tx, "200 type set".into()).await,
            "SIZE" => {
                let asked = arg.trim();
                if asked == FILE_NAME || asked.ends_with(FILE_NAME) {
                    reply(&mut tx, format!("213 {TOTAL}")).await;
                } else {
                    reply(&mut tx, "550 no such file".into()).await;
                }
            }
            "REST" => {
                rest_offset = arg.trim().parse().unwrap_or(0);
                reply(&mut tx, format!("350 restarting at {rest_offset}")).await;
            }
            "EPSV" => {
                if !config.epsv {
                    reply(&mut tx, "500 EPSV not supported, use PASV".into()).await;
                    continue;
                }
                match TcpListener::bind("127.0.0.1:0").await {
                    Ok(listener) => {
                        let port = listener.local_addr().expect("data addr").port();
                        reply(
                            &mut tx,
                            format!("229 Entering Extended Passive Mode (|||{port}|)"),
                        )
                        .await;
                        data_listener = Some(listener);
                    }
                    Err(_) => return,
                }
            }
            "PASV" => match TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => {
                    let port = listener.local_addr().expect("data addr").port();
                    let hi = port >> 8;
                    let lo = port & 0xff;
                    reply(
                        &mut tx,
                        format!("227 Entering Passive Mode (127,0,0,1,{hi},{lo})"),
                    )
                    .await;
                    data_listener = Some(listener);
                }
                Err(_) => return,
            },
            "RETR" => {
                let Some(listener) = data_listener.take() else {
                    reply(&mut tx, "425 no passive mode entered".into()).await;
                    return;
                };
                reply(&mut tx, format!("150 sending {FILE_NAME}")).await;
                let Ok((mut data, _)) = listener.accept().await else {
                    reply(&mut tx, "426 data connection failed".into()).await;
                    return;
                };
                for offset in rest_offset..TOTAL {
                    if data.write_all(&[body_byte(offset)]).await.is_err() {
                        break;
                    }
                }
                let _ = data.flush().await;
                drop(data);
                reply(&mut tx, "226 transfer complete".into()).await;
            }
            "QUIT" => {
                reply(&mut tx, "221 goodbye".into()).await;
                return;
            }
            _ => reply(&mut tx, "502 command not implemented".into()).await,
        }
    }
}

/// Bind a control listener and serve every incoming connection.
async fn serve(listener: TcpListener, config: Config) {
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(handle_control(sock, config));
    }
}

/// Spawn a server on a random port; returns `ftp://<host>:<port>/file.bin`.
async fn spawn_server(config: Config) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(serve(listener, config));
    format!("ftp://{addr}/{FILE_NAME}")
}

fn temp_out(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rg-ftp-{tag}-{}.bin", std::process::id()))
}

#[tokio::test]
async fn anonymous_download_completes() {
    let url = spawn_server(Config::anonymous()).await;
    let out = temp_out("anon");
    let _ = std::fs::remove_file(&out);

    let stats = Download::new(url, out.clone()).run().await.unwrap();
    assert_eq!(stats.total, TOTAL);

    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len() as u64, TOTAL);
    for (offset, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, body_byte(offset as u64), "byte {offset}");
    }
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn wrong_password_maps_to_ftp_auth() {
    let url = spawn_server(Config::auth_only()).await;
    let out = temp_out("wrongpass");
    let _ = std::fs::remove_file(&out);

    // No credentials anywhere: login is refused with 530.
    let error = Download::new(&url, out.clone()).run().await.expect_err("530");
    assert!(matches!(error, Error::FtpAuth(_)), "got {error}");

    // Wrong password via URL userinfo: also 530.
    let error = Download::new(format!("ftp://alfred:nope@{}", url.split_at(6).1), out.clone())
        .run()
        .await
        .expect_err("530");
    assert!(matches!(error, Error::FtpAuth(_)), "got {error}");

    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn credentials_complete_the_download() {
    let url = spawn_server(Config::auth_only()).await;
    let out = temp_out("auth");
    let _ = std::fs::remove_file(&out);

    let stats = Download::new(url, out.clone())
        .network(rg_core::NetworkConfig {
            credentials: Some(Credentials::new(USER, PASS)),
            ..rg_core::NetworkConfig::default()
        })
        .run()
        .await
        .unwrap();

    assert_eq!(stats.total, TOTAL);
    let bytes = std::fs::read(&out).unwrap();
    for (offset, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, body_byte(offset as u64), "byte {offset}");
    }
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn pasv_fallback_when_epsv_rejected() {
    let url = spawn_server(Config::pasv_fallback()).await;
    let out = temp_out("pasv");
    let _ = std::fs::remove_file(&out);

    let stats = Download::new(url, out.clone()).run().await.unwrap();
    assert_eq!(stats.total, TOTAL, "PASV fallback must still transfer");
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes[0], body_byte(0));
    assert_eq!(bytes[TOTAL as usize - 1], body_byte(TOTAL - 1));
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn resumes_from_an_existing_tmp_with_rest() {
    let url = spawn_server(Config::anonymous()).await;
    let out = temp_out("resume");
    let _ = std::fs::remove_file(&out);

    // Manually create the .rgtmp the client resumes from: first kilobyte.
    // The client computes the temp path by appending ".rgtmp" to the output.
    let mut tmp_name = out.clone().into_os_string();
    tmp_name.push(".rgtmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    let existing: u64 = 1024;
    std::fs::write(&tmp, {
        let mut v = Vec::with_capacity(existing as usize);
        for offset in 0..existing {
            v.push(body_byte(offset));
        }
        v
    })
    .unwrap();

    let stats = Download::new(url, out.clone())
        .resume(true)
        .run()
        .await
        .unwrap();

    assert_eq!(stats.total, TOTAL, "total must include the resumed part");
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len() as u64, TOTAL);
    for (offset, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, body_byte(offset as u64), "byte {offset}");
    }
    assert!(!tmp.exists(), "tmp file must be renamed to the output");
    let _ = std::fs::remove_file(&out);
}
