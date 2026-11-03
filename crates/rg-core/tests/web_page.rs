//! HTML responses are web pages, not files. The engine must refuse them (or
//! rename `.bin` to `.html` when explicitly allowed), never silently save
//! YouTube's `/watch` page as a download.

use std::time::Duration;

use rg_core::{Download, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const BODY: &[u8] = b"<html><body>not a video</body></html>";

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

            let is_head = request.starts_with(b"HEAD");
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BODY.len()
            );
            let _ = sock.write_all(header.as_bytes()).await;
            if !is_head {
                let _ = sock.write_all(BODY).await;
            }
            let _ = sock.flush().await;
        });
    }
}

async fn start_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(serve(listener));
    format!("http://{addr}/watch?v=abc")
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rg-core-html-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn refuses_html_by_default() {
    let url = start_server().await;
    let dir = temp_dir("refuse");
    let out = dir.join("download.bin");

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Download::new(url, out.clone()).connections(2).run(),
    )
    .await
    .expect("probe timed out");

    match result {
        Err(Error::WebPage { content_type, .. }) => {
            assert!(content_type.starts_with("text/html"), "got {content_type}");
        }
        other => panic!("expected Error::WebPage, got {other:?}"),
    }
    assert!(!out.exists(), "a refused page must not create a file");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn allow_html_infers_the_extension() {
    let url = start_server().await;
    let dir = temp_dir("allow");
    let out = dir.join("download.bin");

    let stats = tokio::time::timeout(
        Duration::from_secs(10),
        Download::new(url, out.clone())
            .connections(2)
            .allow_html(true)
            .run(),
    )
    .await
    .expect("download timed out")
    .expect("allow_html download failed");

    let expected = dir.join("download.html");
    assert_eq!(stats.output, expected, "extension should come from Content-Type");
    assert!(!out.exists(), "the .bin placeholder must not be written");
    assert_eq!(
        tokio::fs::read(&expected).await.expect("read output"),
        BODY,
        "saved body differs"
    );
    assert_eq!(stats.total, BODY.len() as u64);

    let _ = std::fs::remove_dir_all(&dir);
}
