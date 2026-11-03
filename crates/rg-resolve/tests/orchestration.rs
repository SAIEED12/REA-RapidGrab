//! End-to-end orchestration tests with a fake yt-dlp/ffmpeg and a local HTTP
//! server, so no external tools or network access are required.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rg_resolve::{
    download_media_with, CommandOutput, MediaEvent, MediaRequest, MediaStage, ResolveError, Runner,
    Tools,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct MockRunner {
    metadata: String,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl MockRunner {
    fn new(metadata: String) -> Self {
        Self {
            metadata,
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

        if name.contains("yt-dlp") {
            return Ok(CommandOutput {
                success: true,
                status: Some(0),
                stdout: self.metadata.clone().into_bytes(),
                stderr: Vec::new(),
            });
        }

        if name.contains("ffmpeg") {
            let mut inputs = Vec::new();
            let mut output = None;
            let mut iter = args.iter();
            while let Some(arg) = iter.next() {
                if arg == "-i" {
                    if let Some(input) = iter.next() {
                        inputs.push(PathBuf::from(input));
                    }
                } else if !arg.starts_with('-') && arg != "copy" && arg != "error" {
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
                .unwrap_or_else(|| ("application/octet-stream".to_string(), Vec::new()));
            let is_head = request.starts_with(b"HEAD");

            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            if !is_head {
                // Slow enough that the engine's 500 ms progress tick fires.
                for (index, chunk) in body.chunks(32 * 1024).enumerate() {
                    if index > 0 {
                        tokio::time::sleep(Duration::from_millis(250)).await;
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
        "rg-resolve-orchestration-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn body(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| tag.wrapping_add(i as u8)).collect()
}

fn muxed_metadata(page_url: &str, stream_url: &str) -> String {
    format!(
        r#"{{"title":"Walk Through Text","duration":12.5,"webpage_url":"{page_url}",
            "formats":[
                {{"format_id":"22","ext":"mp4","vcodec":"avc1.640028",
                  "acodec":"mp4a.40.2","height":720,"tbr":2000,
                  "url":"{stream_url}","protocol":"https",
                  "http_headers":{{"User-Agent":"test-agent","Referer":"{page_url}"}}}}
            ]}}"#
    )
}

fn dash_metadata(page_url: &str, video_url: &str, audio_url: &str) -> String {
    format!(
        r#"{{"title":"Merge Clip","duration":30.0,"webpage_url":"{page_url}",
            "formats":[
                {{"format_id":"137","ext":"mp4","vcodec":"avc1.640028",
                  "acodec":"none","height":1080,"tbr":4000,
                  "url":"{video_url}","protocol":"https"}},
                {{"format_id":"140","ext":"m4a","vcodec":"none",
                  "acodec":"mp4a.40.2","abr":128,
                  "url":"{audio_url}","protocol":"https"}}
            ]}}"#
    )
}

fn tools() -> Tools {
    Tools {
        yt_dlp: PathBuf::from("yt-dlp-test"),
        ffmpeg: Some(PathBuf::from("ffmpeg-test")),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_a_muxed_stream_without_ffmpeg() {
    let dir = temp_dir("muxed");
    let data = body(7, 96 * 1024);
    let mut routes = HashMap::new();
    routes.insert(
        "/video.mp4".to_string(),
        ("video/mp4".to_string(), data.clone()),
    );
    let base = start_server(routes).await;
    let page_url = format!("{base}/watch?v=abc");
    let runner = MockRunner::new(muxed_metadata(&page_url, &format!("{base}/video.mp4")));

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_cb = Arc::clone(&events);
    let request = MediaRequest::new(page_url.clone(), dir.join("download.bin")).connections(4);

    let output = download_media_with(&runner, &tools(), request, move |event| {
        events_for_cb.lock().expect("events lock").push(event);
    })
    .await
    .expect("muxed download");

    assert_eq!(output, dir.join("Walk Through Text.mp4"));
    assert_eq!(
        std::fs::read(&output).expect("read output"),
        data,
        "stream bytes differ"
    );
    assert!(!dir.join("download.bin").exists());
    assert!(!dir.join("Walk Through Text.mp4.rgparts").exists());

    let events = events.lock().expect("events lock");
    assert!(matches!(events.first(), Some(MediaEvent::Resolving)));
    assert!(events.iter().any(
        |event| matches!(event, MediaEvent::Resolved { needs_merge: false, .. })
    ));
    let progress: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            MediaEvent::Progress(progress) => Some(progress),
            _ => None,
        })
        .collect();
    assert!(!progress.is_empty(), "expected at least one progress event");
    assert!(progress.iter().all(|p| p.stage == MediaStage::Video));
    assert!(progress
        .iter()
        .all(|p| (0.0..=1.0).contains(&p.overall)));
    assert!(matches!(events.last(), Some(MediaEvent::Done { bytes, .. }) if *bytes == data.len() as u64));

    let calls = runner.calls();
    assert_eq!(calls.len(), 1, "muxed download must not invoke ffmpeg");
    assert_eq!(calls[0].0, "yt-dlp-test");
    assert!(calls[0].1.iter().any(|arg| arg == "-J"));
    assert!(calls[0].1.iter().any(|arg| arg == &page_url));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_video_and_audio_then_merges() {
    let dir = temp_dir("merge");
    let video = body(11, 96 * 1024);
    let audio = body(23, 96 * 1024);
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
    let page_url = format!("{base}/watch?v=xyz");
    let runner = MockRunner::new(dash_metadata(
        &page_url,
        &format!("{base}/video.mp4"),
        &format!("{base}/audio.m4a"),
    ));

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_cb = Arc::clone(&events);
    let request = MediaRequest::new(page_url, dir.join("download.bin")).connections(4);

    let output = download_media_with(&runner, &tools(), request, move |event| {
        events_for_cb.lock().expect("events lock").push(event);
    })
    .await
    .expect("merged download");

    assert_eq!(output, dir.join("Merge Clip.mp4"));
    let mut expected = video;
    expected.extend(audio);
    assert_eq!(
        std::fs::read(&output).expect("read output"),
        expected,
        "merged bytes differ"
    );

    assert!(!dir.join(".Merge Clip.rgvideo.mp4").exists());
    assert!(!dir.join(".Merge Clip.rgaudio.m4a").exists());
    assert!(!dir.join(".Merge Clip.rgvideo.mp4.rgparts").exists());
    assert!(!dir.join(".Merge Clip.rgaudio.m4a.rgparts").exists());

    let events = events.lock().expect("events lock");
    assert!(events.iter().any(
        |event| matches!(event, MediaEvent::Resolved { needs_merge: true, .. })
    ));
    assert!(events.iter().any(|event| matches!(event, MediaEvent::Merging)));

    let progress: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            MediaEvent::Progress(progress) => Some(progress),
            _ => None,
        })
        .collect();
    assert!(progress
        .iter()
        .any(|p| p.stage == MediaStage::Video && p.overall <= 0.80));
    assert!(progress
        .iter()
        .any(|p| p.stage == MediaStage::Audio && p.overall >= 0.80));
    assert!(progress.iter().all(|p| (0.0..=0.97).contains(&p.overall)));

    let calls = runner.calls();
    assert_eq!(calls.len(), 2, "expected resolve + merge");
    assert!(calls[1].0.contains("ffmpeg"));
    assert!(calls[1].1.iter().any(|arg| arg == "-c"));
    assert!(calls[1].1.iter().any(|arg| arg == "copy"));
    assert!(calls[1].1.iter().any(|arg| arg == "+faststart"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_missing_ffmpeg_before_downloading() {
    let dir = temp_dir("no-ffmpeg");
    let mut routes = HashMap::new();
    routes.insert(
        "/video.mp4".to_string(),
        ("video/mp4".to_string(), body(3, 1024)),
    );
    routes.insert(
        "/audio.m4a".to_string(),
        ("audio/mp4".to_string(), body(4, 512)),
    );
    let base = start_server(routes).await;
    let runner = MockRunner::new(dash_metadata(
        &format!("{base}/watch?v=none"),
        &format!("{base}/video.mp4"),
        &format!("{base}/audio.m4a"),
    ));
    let tools = Tools {
        yt_dlp: PathBuf::from("yt-dlp-test"),
        ffmpeg: None,
    };

    let error = download_media_with(
        &runner,
        &tools,
        MediaRequest::new(format!("{base}/watch?v=none"), dir.join("download.bin")),
        |_| {},
    )
    .await
    .expect_err("missing ffmpeg must fail");

    assert!(matches!(error, ResolveError::FfmpegMissing));
    assert_eq!(runner.calls().len(), 1, "only yt-dlp should have run");
    assert!(!dir.join("Merge Clip.mp4").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn honours_a_pre_set_cancel_flag() {
    let dir = temp_dir("cancel");
    let runner = MockRunner::new("{}".to_string());
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));

    let error = download_media_with(
        &runner,
        &tools(),
        MediaRequest::new("https://example.com/watch", dir.join("download.bin"))
            .cancel_flag(cancel),
        |_| {},
    )
    .await
    .expect_err("cancelled request must fail");

    assert!(error.is_cancelled());
    assert!(runner.calls().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn surfaces_yt_dlp_failures() {
    struct FailingRunner;

    #[async_trait]
    impl Runner for FailingRunner {
        async fn run(
            &self,
            _program: &Path,
            _args: &[String],
        ) -> Result<CommandOutput, ResolveError> {
            Ok(CommandOutput {
                success: false,
                status: Some(1),
                stdout: Vec::new(),
                stderr: b"ERROR: Video unavailable".to_vec(),
            })
        }
    }

    let dir = temp_dir("yt-dlp-error");
    let error = download_media_with(
        &FailingRunner,
        &tools(),
        MediaRequest::new("https://example.com/watch", dir.join("download.bin")),
        |_| {},
    )
    .await
    .expect_err("yt-dlp failure must propagate");

    match error {
        ResolveError::Tool { tool, message } => {
            assert_eq!(tool, "yt-dlp");
            assert!(message.contains("Video unavailable"), "got {message}");
            assert!(
                message.contains("--cookies-from-browser"),
                "unavailable videos should suggest cookies: {message}"
            );
        }
        other => panic!("expected Tool error, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}
