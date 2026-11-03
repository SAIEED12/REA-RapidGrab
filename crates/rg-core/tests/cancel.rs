//! Cancellation and resume behaviour against a tiny in-process HTTP server.
//!
//! The server serves deterministic bytes (`byte = offset % 251`) slowly enough
//! that a cancel lands mid-transfer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rg_core::{parts_dir_for, Download, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOTAL: u64 = 4 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const CHUNK_DELAY: Duration = Duration::from_millis(40);

fn body_byte(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn parse_range(request: &str) -> Option<(u64, u64)> {
    for line in request.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range: bytes=") {
            let (start, end) = rest.split_once('-')?;
            let start: u64 = start.trim().parse().ok()?;
            let end: u64 = if end.trim().is_empty() {
                TOTAL - 1
            } else {
                end.trim().parse().ok()?
            };
            return Some((start, end.min(TOTAL - 1)));
        }
    }
    None
}

async fn serve(listener: TcpListener, ranges: Arc<Mutex<Vec<u64>>>) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let ranges = ranges.clone();
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
            let request = String::from_utf8_lossy(&request).to_string();
            let is_head = request.starts_with("HEAD");

            if is_head {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
                return;
            }

            let (start, end) = parse_range(&request).unwrap_or((0, TOTAL - 1));
            ranges.lock().expect("ranges lock").push(start);
            let len = end.saturating_sub(start) + 1;
            let header = if start == 0 && end == TOTAL - 1 && parse_range(&request).is_none() {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{TOTAL}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                )
            };
            if sock.write_all(header.as_bytes()).await.is_err() {
                return;
            }

            let mut offset = start;
            while offset <= end {
                let n = ((end - offset + 1) as usize).min(CHUNK);
                let chunk: Vec<u8> = (0..n).map(|i| body_byte(offset + i as u64)).collect();
                if sock.write_all(&chunk).await.is_err() {
                    return;
                }
                offset += n as u64;
                tokio::time::sleep(CHUNK_DELAY).await;
            }
            let _ = sock.flush().await;
        });
    }
}

async fn start_server() -> String {
    start_server_logged().await.0
}

async fn start_server_logged() -> (String, Arc<Mutex<Vec<u64>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let ranges = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(serve(listener, ranges.clone()));
    (format!("http://{addr}/file.bin"), ranges)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rg-core-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn cancel_stops_quickly_and_keeps_parts() {
    let url = start_server().await;
    let dir = temp_dir("cancel");
    let out = dir.join("file.bin");

    let flag = Arc::new(AtomicBool::new(false));
    let download = Download::new(url.clone(), out.clone())
        .connections(4)
        .cancel_flag(flag.clone());

    let handle = tokio::spawn(async move { download.run().await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    flag.store(true, Ordering::Relaxed);

    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("cancel did not stop the download in time")
        .expect("task panicked");

    assert!(
        matches!(result, Err(Error::Cancelled)),
        "expected Error::Cancelled, got {result:?}"
    );
    assert!(
        !out.exists(),
        "cancelled download must not produce the final file"
    );

    let parts = parts_dir_for(&out);
    assert!(parts.exists(), "part files must survive a cancel");
    assert!(
        std::fs::read_dir(&parts)
            .expect("read parts dir")
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("part_")),
        "expected at least one part file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resume_after_cancel_is_byte_exact() {
    let url = start_server().await;
    let dir = temp_dir("resume");
    let out = dir.join("file.bin");

    let flag = Arc::new(AtomicBool::new(false));
    let download = Download::new(url.clone(), out.clone())
        .connections(4)
        .cancel_flag(flag.clone());
    let handle = tokio::spawn(async move { download.run().await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    flag.store(true, Ordering::Relaxed);
    let cancelled = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("cancel timed out")
        .expect("task panicked");
    assert!(matches!(cancelled, Err(Error::Cancelled)));

    let stats = Download::new(url, out.clone())
        .connections(4)
        .run()
        .await
        .expect("resume failed");
    assert_eq!(stats.total, TOTAL);

    let data = tokio::fs::read(&out).await.expect("read output");
    assert_eq!(data.len() as u64, TOTAL, "wrong final size");
    for (i, byte) in data.iter().enumerate() {
        assert_eq!(*byte, body_byte(i as u64), "byte {i} differs");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resume_key_survives_a_changed_signed_url() {
    let (base, ranges) = start_server_logged().await;
    let dir = temp_dir("resume-key");
    let out = dir.join("file.bin");
    let key = "page|video|137";

    let flag = Arc::new(AtomicBool::new(false));
    let first = Download::new(format!("{base}?sig=one"), out.clone())
        .connections(4)
        .resume_key(key)
        .cancel_flag(flag.clone());
    let handle = tokio::spawn(async move { first.run().await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    flag.store(true, Ordering::Relaxed);
    let cancelled = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("cancel timed out")
        .expect("task panicked");
    assert!(matches!(cancelled, Err(Error::Cancelled)));
    assert!(parts_dir_for(&out).exists(), "parts must survive the cancel");

    ranges.lock().expect("ranges lock").clear();

    let stats = Download::new(format!("{base}?sig=two"), out.clone())
        .connections(4)
        .resume_key(key)
        .run()
        .await
        .expect("resume with a new signed URL failed");
    assert_eq!(stats.total, TOTAL);

    let starts = ranges.lock().expect("ranges lock").clone();
    assert!(
        starts.iter().any(|&start| start > 0),
        "new signed URL restarted from zero instead of resuming: {starts:?}"
    );

    let data = tokio::fs::read(&out).await.expect("read output");
    assert_eq!(data.len() as u64, TOTAL);
    for (i, byte) in data.iter().enumerate() {
        assert_eq!(*byte, body_byte(i as u64), "byte {i} differs");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_flag_set_before_start_returns_immediately() {
    let dir = temp_dir("precancel");
    let out = dir.join("file.bin");
    let flag = Arc::new(AtomicBool::new(true));

    let result = Download::new("http://127.0.0.1:1/never", out)
        .cancel_flag(flag)
        .run()
        .await;

    assert!(matches!(result, Err(Error::Cancelled)), "got {result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
