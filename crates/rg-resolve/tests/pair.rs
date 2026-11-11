//! End-to-end tests for the browser-captured DASH video+audio pair: one
//! local HTTP server serves both renditions, a mock ffmpeg "muxes" them by
//! concatenation, so no external tools or network access are required.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rg_resolve::{
    download_pair_with, CommandOutput, MediaEvent, MediaStage, PairRequest, ResolveError, Runner,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct MockRunner {
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl MockRunner {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().expect("calls lock").clone()
    }
}

#[async_trait]
impl Runner for MockRunner {
    async fn run(&self, program: &Path, args: &[String]) -> Result<CommandOutput, ResolveError> {
        let name = program
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        self.calls
            .lock()
            .expect("calls lock")
            .push((name.clone(), args.to_vec()));

        if name.contains("ffmpeg") {
            let mut inputs = Vec::new();
            let mut output = None;
            let mut iter = args.iter();
            while let Some(arg) = iter.next() {
                if arg == "-i" {
                    if let Some(input) = iter.next() {
                        inputs.push(PathBuf::from(input));
                    }
                } else if !arg.starts_with('-')
                    && arg != "copy"
                    && arg != "error"
                    && arg != "0:v:0"
                    && arg != "1:a:0"
                {
                    output = Some(PathBuf::from(arg));
                }
            }
            let output = output.expect("ffmpeg output path");
            let mut merged = Vec::new();
            for input in inputs {
                merged.extend(std::fs::read(input).expect("read input"));
            }
            std::fs::write(&output, &merged).expect("write merged output");
            return Ok(CommandOutput {
                success: true,
                status: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }

        Ok(CommandOutput {
            success: false,
            status: Some(127),
            stdout: Vec::new(),
            stderr: b"unknown tool".to_vec(),
        })
    }
}

async fn serve(listener: TcpListener, routes: HashMap<String, (String, Vec<u8>)>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let routes = routes.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            let mut request = Vec::new();
            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            let text = String::from_utf8_lossy(&request);
            let target = text.split_whitespace().nth(1).unwrap_or("/");
            let path = target.split('?').next().unwrap_or("/");
            let (content_type, body) = routes
                .get(path)
                .cloned()
                .unwrap_or_else(|| ("text/html".to_string(), b"<!doctype html>".to_vec()));
            let not_found = !routes.contains_key(path);
            let is_head = request.starts_with(b"HEAD");

            let header = if not_found {
                format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
            };
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            if !is_head {
                for (index, chunk) in body.chunks(32 * 1024).enumerate() {
                    if index > 0 {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                }
            }
            let _ = socket.flush().await;
        });
    }
}

async fn start_server(routes: HashMap<String, (String, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(serve(listener, routes));
    format!("http://{addr}")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rg-resolve-pair-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn body(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| tag.wrapping_add(i as u8)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_both_tracks_and_merges_them() {
    let dir = temp_dir("merge");
    let video = body(7, 96 * 1024);
    let audio = body(13, 24 * 1024);
    let mut routes = HashMap::new();
    routes.insert(
        "/video.mp4".to_string(),
        ("video/mp4".to_string(), video.clone()),
    );
    routes.insert(
        "/audio.m4a".to_string(),
        ("audio/mp4".to_string(), audio.clone()),
    );
    let base = start_server(routes).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_cb = Arc::clone(&events);
    let request = PairRequest::new(
        format!("{base}/reel/1"),
        format!("{base}/video.mp4"),
        format!("{base}/audio.m4a"),
        dir.join("clip.mp4"),
    );

    let ffmpeg = PathBuf::from("ffmpeg-test");
    // Keep the runner so we can inspect the ffmpeg invocation afterwards.
    let runner = MockRunner::new();
    let output = download_pair_with(&runner, &ffmpeg, request, move |event| {
        events_for_cb.lock().expect("events lock").push(event);
    })
    .await
    .expect("paired download");

    assert_eq!(output, dir.join("clip.mp4"));
    let mut merged = video.clone();
    merged.extend(audio.iter());
    assert_eq!(
        std::fs::read(&output).expect("read output"),
        merged,
        "the merged file must be video then audio"
    );

    // The sidecar files and their segment parts are gone after the merge.
    let hidden = |name: &str| dir.join(name);
    assert!(!hidden(".clip.rgvideo.mp4").exists());
    assert!(!hidden(".clip.rgaudio.m4a").exists());
    assert!(!hidden(".clip.rgvideo.mp4.rgparts").exists());
    assert!(!hidden(".clip.rgaudio.m4a.rgparts").exists());

    let calls = runner.calls();
    let ffmpeg_calls: Vec<_> = calls
        .iter()
        .filter(|(name, _)| name.contains("ffmpeg"))
        .collect();
    assert_eq!(ffmpeg_calls.len(), 1, "the merge must start exactly one ffmpeg");
    let args = &ffmpeg_calls[0].1;
    let maps: Vec<&str> = args
        .windows(2)
        .filter(|pair| pair[0] == "-map")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(maps, ["0:v:0", "1:a:0"], "the merge keeps one video and one audio track");
    let events = events.lock().expect("events lock");
    let overshoot = events
        .iter()
        .filter_map(|event| match event {
            MediaEvent::Progress(p) => Some(p.overall),
            _ => None,
        })
        .last()
        .expect("at least one progress event");
    assert!(overshoot <= 1.0, "progress must stay within 0..1");
    assert!(events
        .iter()
        .filter_map(|event| match event {
            MediaEvent::Progress(p) => Some(p.stage),
            _ => None,
        })
        .all(|stage| matches!(stage, MediaStage::Video | MediaStage::Audio | MediaStage::Merging)));
    assert!(events.iter().any(|event| matches!(
        event,
        MediaEvent::Progress(_) | MediaEvent::Merging
    )));
    assert!(matches!(events.last(), Some(MediaEvent::Done { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_sidecars_when_the_merge_fails() {
    let dir = temp_dir("merge-fail");
    let video = body(7, 32 * 1024);
    let audio = body(13, 16 * 1024);
    let mut routes = HashMap::new();
    routes.insert(
        "/video.mp4".to_string(),
        ("video/mp4".to_string(), video.clone()),
    );
    routes.insert(
        "/audio.m4a".to_string(),
        ("audio/mp4".to_string(), audio.clone()),
    );
    let base = start_server(routes).await;

    struct FailingRunner;

    #[async_trait]
    impl Runner for FailingRunner {
        async fn run(
            &self,
            program: &Path,
            _args: &[String],
        ) -> Result<CommandOutput, ResolveError> {
            let name = program
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(CommandOutput {
                success: false,
                status: Some(1),
                stdout: Vec::new(),
                stderr: format!("{name} said no: matches no streams").into_bytes(),
            })
        }
    }

    let request = PairRequest::new(
        format!("{base}/reel/2"),
        format!("{base}/video.mp4"),
        format!("{base}/audio.m4a"),
        dir.join("fail.mp4"),
    )
    .connections(2);

    let err = download_pair_with(&FailingRunner, &PathBuf::from("ffmpeg-test"), request, |_| {})
        .await
        .expect_err("merge must fail");
    assert!(err.to_string().contains("ffmpeg"), "unexpected: {err}");
    // The downloads survive a failed merge so Retry resumes them.
    assert!(dir.join(".fail.rgvideo.mp4").exists());
    assert!(dir.join(".fail.rgaudio.m4a").exists());
    assert!(!dir.join("fail.mp4").exists(), "failed merge must not produce an output");
}

#[tokio::test(flavor = "multi_thread")]
async fn fails_fast_when_the_audio_track_is_gone() {
    let dir = temp_dir("audio-gone");
    let video = body(7, 32 * 1024);
    let mut routes = HashMap::new();
    routes.insert(
        "/video.mp4".to_string(),
        ("video/mp4".to_string(), video.clone()),
    );
    let base = start_server(routes).await;

    let request = PairRequest::new(
        format!("{base}/reel/3"),
        format!("{base}/video.mp4"),
        format!("{base}/audio.m4a"),
        dir.join("gone.mp4"),
    )
    .connections(2);

    let err = download_pair_with(&MockRunner::new(), &PathBuf::from("ffmpeg-test"), request, |_| {})
        .await
        .expect_err("audio download must fail");
    assert!(matches!(err, ResolveError::Core(_)), "unexpected: {err}");
    assert!(dir.join(".gone.rgvideo.mp4").exists(), "video is resumable");
    assert!(!dir.join("gone.mp4").exists());
}
