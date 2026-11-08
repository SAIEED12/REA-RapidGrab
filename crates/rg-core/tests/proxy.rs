//! Proxy routing: manual HTTP proxy (absolute-form requests) and the
//! no-proxy bypass list, against a tiny in-process forwarding proxy.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rg_core::{Download, NetworkConfig, ProxyConfig, ProxyMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOTAL: u64 = 2 * 1024 * 1024;

fn body_byte(offset: u64) -> u8 {
    (offset % 251) as u8
}

/// Plain HTTP target server: one ranged GET per connection, then close.
async fn serve_target(listener: TcpListener) {
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
            let range = request
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("range: bytes=")
                        .map(|value| value.trim().to_string())
                })
                .unwrap_or_else(|| format!("0-{}", TOTAL - 1));
            let (start, end) = {
                let (start, end) = range.split_once('-').unwrap_or(("0", ""));
                let start = start.parse::<u64>().unwrap_or(0);
                let end = if end.is_empty() {
                    TOTAL - 1
                } else {
                    end.parse::<u64>().unwrap_or(TOTAL - 1).min(TOTAL - 1)
                };
                (start, end)
            };
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
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

/// Forwarding proxy: records how many requests it saw and pipes the rest.
async fn serve_proxy(listener: TcpListener, target: String, hits: Arc<AtomicUsize>) {
    let (target_host, target_port) = target.split_once(':').expect("host:port");
    loop {
        let Ok((mut client_sock, _)) = listener.accept().await else {
            return;
        };
        let target_host = target_host.to_string();
        let target_port = target_port.parse::<u16>().unwrap();
        let hits = hits.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            let mut request = Vec::new();
            loop {
                let n = match client_sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&request);
            let mut lines = text.lines();
            let request_line = lines.next().unwrap_or_default().to_string();
            let absolute_url = request_line
                .split(' ')
                .nth(1)
                .unwrap_or_default()
                .to_string();
            if !absolute_url.starts_with("http://") {
                return; // not an absolute-form HTTP request
            }
            hits.fetch_add(1, Ordering::SeqCst);

            // Rewrite to origin-form and forward to the target.
            let path = match absolute_url.split_once("://") {
                Some((_, rest)) => rest.split_once('/').map(|(_, path)| path).unwrap_or(""),
                None => return,
            };
            let mut forwarded = format!("GET /{path} HTTP/1.1\r\n");
            for line in lines {
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("proxy-") || lower.starts_with("host:") {
                    continue;
                }
                forwarded.push_str(line);
                forwarded.push_str("\r\n");
            }
            forwarded.push_str(&format!("Host: {target_host}:{target_port}\r\n\r\n"));

            let Ok(mut target_sock) = TcpStream::connect((target_host.as_str(), target_port)).await
            else {
                return;
            };
            if target_sock.write_all(forwarded.as_bytes()).await.is_err() {
                return;
            }
            // Pipe the response back, then keep tunnelling until EOF.
            let mut back = client_sock;
            let mut up = target_sock;
            let _ = tokio::io::copy(&mut up, &mut back).await;
        });
    }
}

fn manual_proxy(address: &str, no_proxy: &str) -> NetworkConfig {
    NetworkConfig {
        proxy: ProxyConfig {
            mode: ProxyMode::Manual,
            url: address.to_string(),
            no_proxy: no_proxy.to_string(),
            ..ProxyConfig::default()
        },
        credentials: None,
    }
}

#[tokio::test]
async fn downloads_route_through_the_proxy() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap().to_string();
    let proxy_addr = proxy.local_addr().unwrap().to_string();
    tokio::spawn(serve_target(target));
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve_proxy(proxy, target_addr.clone(), hits.clone()));

    let out = std::env::temp_dir().join(format!("rg-proxy-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let stats = Download::new(format!("http://{target_addr}/file.bin"), out.clone())
        .connections(2)
        .network(manual_proxy(&proxy_addr, ""))
        .run()
        .await
        .unwrap();

    assert_eq!(stats.total, TOTAL);
    assert!(hits.load(Ordering::SeqCst) >= 1, "proxy saw no request");
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len() as u64, TOTAL);
    for (offset, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, body_byte(offset as u64), "byte {offset}");
    }
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn no_proxy_bypasses_the_proxy() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap().to_string();
    let proxy_addr = proxy.local_addr().unwrap().to_string();
    tokio::spawn(serve_target(target));
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve_proxy(proxy, target_addr.clone(), hits.clone()));

    let out = std::env::temp_dir().join(format!("rg-noproxy-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    Download::new(format!("http://{target_addr}/file.bin"), out.clone())
        .connections(1)
        .network(manual_proxy(&proxy_addr, "127.0.0.1, localhost"))
        .run()
        .await
        .unwrap();

    // Give a (wrong) proxy hop a moment to show up; it never should.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "bypassed host reached the proxy"
    );
    assert_eq!(std::fs::metadata(&out).unwrap().len(), TOTAL);
    let _ = std::fs::remove_file(&out);
}
