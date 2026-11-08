//! HTTP Basic authentication: a 401 must surface as `Error::AuthRequired`
//! (the GUI opens its credentials dialog), and preemptive credentials must
//! complete the download.

use rg_core::{Credentials, Download, Error, NetworkConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOTAL: u64 = 1024 * 1024;
const USER: &str = "alfred";
const PASS: &str = "s3cret";
const REALM: &str = "RapidGrab Test";

fn body_byte(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn expected_header() -> String {
    // base64("alfred:s3cret") — precomputed constant.
    format!("Basic {}", b64(b"alfred:s3cret"))
}

/// Minimal base64 (standard alphabet, padding).
fn b64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Server that answers every request with 401 unless the exact
/// `Authorization` header is present.
async fn serve(listener: TcpListener) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut request = Vec::new();
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let authorized = request.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("authorization: {}", expected_header()))
            });
            let (start, end) = match request
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("range: bytes=")
                        .map(|value| value.trim().to_string())
                }) {
                Some(range) => {
                    let (start, end) = range.split_once('-').unwrap_or(("0", ""));
                    let start = start.parse::<u64>().unwrap_or(0);
                    let end = if end.is_empty() {
                        TOTAL - 1
                    } else {
                        end.parse::<u64>().unwrap_or(TOTAL - 1).min(TOTAL - 1)
                    };
                    (start, end)
                }
                None => (0, TOTAL - 1),
            };

            if !authorized {
                let head = format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"{REALM}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
                return;
            }

            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                end - start + 1
            );
            let _ = sock.write_all(head.as_bytes()).await;
            for offset in start..=end {
                if sock.write_all(&[body_byte(offset)]).await.is_err() {
                    return;
                }
            }
            let _ = sock.flush().await;
        });
    }
}

fn network_with(user: Option<&str>, pass: Option<&str>) -> NetworkConfig {
    NetworkConfig {
        credentials: user.map(|user| Credentials::new(user, pass.unwrap_or_default())),
        ..NetworkConfig::default()
    }
}

#[tokio::test]
async fn missing_credentials_surface_as_auth_required() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve(listener));

    let out = std::env::temp_dir().join(format!("rg-auth-missing-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let error = Download::new(format!("http://{addr}/file.bin"), out)
        .network(network_with(None, None))
        .run()
        .await
        .expect_err("401 must fail the download");

    match error {
        Error::AuthRequired {
            status, realm, ..
        } => {
            assert_eq!(status, 401);
            assert_eq!(realm.as_deref(), Some(REALM));
        }
        other => panic!("expected AuthRequired, got {other}"),
    }
}

#[tokio::test]
async fn wrong_credentials_still_report_auth_required() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve(listener));

    let out = std::env::temp_dir().join(format!("rg-auth-wrong-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let error = Download::new(format!("http://{addr}/file.bin"), out)
        .network(network_with(Some("alfred"), Some("nope")))
        .run()
        .await
        .expect_err("wrong credentials must fail");

    assert!(matches!(error, Error::AuthRequired { status: 401, .. }));
}

#[tokio::test]
async fn correct_credentials_complete_the_download() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve(listener));

    let out = std::env::temp_dir().join(format!("rg-auth-ok-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let stats = Download::new(format!("http://{addr}/file.bin"), out.clone())
        .connections(2)
        .network(network_with(Some(USER), Some(PASS)))
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
