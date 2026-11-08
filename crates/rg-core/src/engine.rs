//! IDM-style multi-segment download engine.
//!
//! Behaviour was derived from observed IDM 6.41 behaviour (see README):
//! * probe capabilities, then split the remaining range at its midpoint,
//! * keep up to N workers busy, each new worker splitting the largest
//!   remaining segment (the finished connection "steals" half of it),
//! * one part file per segment, resumed from a persisted manifest,
//! * reconnect immediately on failure and continue at the exact byte offset,
//! * merge part files in order and verify the final size.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{ACCEPT_ENCODING, COOKIE, IF_RANGE, RANGE, REFERER, USER_AGENT};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::{Mutex, Notify};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::limiter::Limiter;
use crate::manifest::{parts_dir_for, Manifest, Segment, MANIFEST_VERSION};
use crate::net::{Credentials, NetworkConfig};
use crate::probe;

pub const DEFAULT_CONNECTIONS: usize = 8;
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Minimum number of bytes handed to a newly created tail segment.
///
/// Ghidra shows IDM's `FUN_0055a820` ("Insert next chunk in list") using
/// `0x2da0` (11,680 bytes) as the floor for a thin tail and `0x5b40`
/// (23,360 bytes) for larger remainders.
const MIN_CHUNK_BYTES: u64 = 0x2da0;

/// Remaining bytes below which IDM stops splitting (`0xb681` in `FUN_0055a820`).
const MIN_SPLIT_BYTES: u64 = 0xb681;

#[derive(Debug, Clone)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub connections: usize,
    /// Path the engine is actually writing to (it may refine `download.bin`).
    pub output: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub total: u64,
    pub elapsed: Duration,
    pub segments: usize,
    pub ranged: bool,
    /// Final path written; inferred extensions can differ from the requested path.
    pub output: PathBuf,
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// Builder for one download.
pub struct Download {
    url: String,
    output: PathBuf,
    connections: usize,
    speed_limit: Option<u64>,
    user_agent: String,
    referer: Option<String>,
    cookie: Option<String>,
    resume_key: Option<String>,
    resume: bool,
    retry_base: Duration,
    retry_max: Duration,
    expected_sha256: Option<String>,
    on_progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
    allow_html: bool,
    network: NetworkConfig,
}

impl Download {
    pub fn new(url: impl Into<String>, output: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            output: output.into(),
            connections: DEFAULT_CONNECTIONS,
            speed_limit: None,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            referer: None,
            cookie: None,
            resume_key: None,
            resume: true,
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(5),
            expected_sha256: None,
            on_progress: None,
            cancel: None,
            allow_html: false,
            network: NetworkConfig::default(),
        }
    }

    pub fn connections(mut self, n: usize) -> Self {
        self.connections = n.max(1);
        self
    }

    pub fn speed_limit(mut self, bytes_per_sec: u64) -> Self {
        self.speed_limit = Some(bytes_per_sec);
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Override the `Referer` header; by default it is derived from the URL.
    pub fn referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }

    /// Send a `Cookie` header on every request (probe, ranges, single stream).
    /// Used for browser captures where the site requires session cookies.
    pub fn cookie(mut self, cookie: impl Into<String>) -> Self {
        let value = cookie.into();
        self.cookie = if value.trim().is_empty() {
            None
        } else {
            Some(value)
        };
        self
    }

    /// Stable resume identity when the request URL is signed and changes
    /// between runs (for example YouTube media URLs).
    pub fn resume_key(mut self, key: impl Into<String>) -> Self {
        self.resume_key = Some(key.into());
        self
    }

    pub fn resume(mut self, yes: bool) -> Self {
        self.resume = yes;
        self
    }

    /// Verify the finished file against an expected SHA-256 (hex, any case).
    ///
    /// IDM performs no content integrity check at all — it only validates that
    /// its part-file chain is consistent — so this is a deliberate improvement.
    pub fn sha256(mut self, expected: impl Into<String>) -> Self {
        let value = expected.into();
        let trimmed = value.trim().to_string();
        self.expected_sha256 = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        self
    }

    pub fn on_progress<F>(mut self, f: F) -> Self
    where
        F: Fn(Progress) + Send + Sync + 'static,
    {
        self.on_progress = Some(Arc::new(f));
        self
    }

    /// Stop the download when `flag` becomes `true`.
    ///
    /// Cancelling is cooperative: workers notice the flag between chunks, keep
    /// the part files and manifest intact (so a later `run()` resumes from the
    /// exact offsets), and never touch the final output file. `run()` then
    /// returns [`Error::Cancelled`].
    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    /// Save HTML responses instead of rejecting them as web pages.
    ///
    /// By default a `text/html` response is refused with [`Error::WebPage`]:
    /// URL-based downloaders are often pointed at pages (YouTube's `/watch`,
    /// for example) where saving the HTML is never what the user wanted.
    pub fn allow_html(mut self, yes: bool) -> Self {
        self.allow_html = yes;
        self
    }

    /// Proxy and other network-layer settings for this download.
    pub fn network(mut self, network: NetworkConfig) -> Self {
        self.network = network;
        self
    }

    pub async fn run(self) -> Result<Stats> {
        let started = Instant::now();
        let cancel = self.cancel.clone();
        if is_cancelled(cancel.as_ref()) {
            return Err(Error::Cancelled);
        }
        let expected_sha256 = self.expected_sha256.clone();

        // FTP has its own path; everything below is HTTP-only.
        if let Ok(scheme_url) = reqwest::Url::parse(&self.url) {
            match scheme_url.scheme() {
                "ftp" => return self.run_ftp(started, cancel, expected_sha256).await,
                "ftps" => return Err(Error::Ftp("ftps:// is not supported".into())),
                _ => {}
            }
        }

        let client = self.network.client_for(&self.url, self.connections).await?;
        let credentials = self
            .network
            .credentials
            .clone()
            .filter(|credentials| !credentials.is_empty());

        let p = probe::probe_with(
            &client,
            &self.url,
            &self.user_agent,
            self.cookie.as_deref(),
            credentials.as_ref(),
        )
        .await?;
        info!(
            "probe: size={} ranges={} etag={:?} type={:?}",
            p.total, p.accept_ranges, p.etag, p.content_type
        );

        if !self.allow_html {
            if let Some(content_type) = p.content_type.as_deref() {
                if crate::naming::is_web_page(content_type) {
                    return Err(Error::WebPage {
                        content_type: content_type.to_string(),
                        url: p.final_url.clone(),
                    });
                }
            }
        }

        let output = crate::naming::infer_output_path(
            &self.output,
            p.content_type.as_deref(),
            p.content_disposition.as_deref(),
        );
        if output != self.output {
            info!(
                "saving to {} (inferred from the response; requested {})",
                output.display(),
                self.output.display()
            );
        }
        let referer = self.referer.clone().or_else(|| referer_for(&p.final_url));

        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await?;
            }
        }

        // No range support (or unknown length): plain single-stream download.
        if !p.accept_ranges || p.total == 0 {
            info!("server has no usable range support; using a single connection");
            let stats = single_stream(
                &client,
                &p.final_url,
                &output,
                &self.user_agent,
                referer.as_deref(),
                self.cookie.as_deref(),
                credentials.as_ref(),
                p.total,
                self.speed_limit,
                cancel.clone(),
                self.on_progress.clone(),
                started,
            )
            .await?;
            if let Some(expected) = &expected_sha256 {
                verify_sha256(&output, expected, cancel.as_ref()).await?;
            }
            return Ok(stats);
        }

        let parts_dir = parts_dir_for(&output);
        fs::create_dir_all(&parts_dir).await?;
        let manifest_path = parts_dir.join("manifest.json");

        let mut st = SegState {
            segments: Vec::new(),
            next_id: 1,
        };
        let mut fresh = true;

        if self.resume {
            if let Some(m) = Manifest::load(&manifest_path)? {
                let same_identity = match (&m.resume_key, &self.resume_key) {
                    (Some(old), Some(new)) => old == new,
                    _ => m.url == p.final_url,
                };
                if same_identity && m.total == p.total && !m.segments.is_empty() {
                    if let (Some(old), Some(new)) = (&m.etag, &p.etag) {
                        if old != new {
                            return Err(Error::RemoteChanged);
                        }
                    }
                    if let (Some(old), Some(new)) = (&m.last_modified, &p.last_modified) {
                        if old != new {
                            return Err(Error::RemoteChanged);
                        }
                    }
                    st.segments = m.segments;
                    st.next_id = st.segments.iter().map(|s| s.id + 1).max().unwrap_or(1);
                    fresh = false;
                    info!("resuming with {} segment(s)", st.segments.len());
                }
            }
        }

        if fresh {
            let _ = fs::remove_dir_all(&parts_dir).await;
            fs::create_dir_all(&parts_dir).await?;
            st.segments = vec![Segment::new(0, 0, p.total - 1)];
            st.next_id = 1;
        } else {
            for s in st.segments.iter_mut() {
                let expected = s.pos.saturating_sub(s.start);
                let path = parts_dir.join(format!("part_{:04}", s.id));
                match fs::metadata(&path).await {
                    Ok(md) if md.len() > expected => {
                        let f = OpenOptions::new().write(true).open(&path).await?;
                        f.set_len(expected).await?;
                    }
                    Ok(_) => {}
                    Err(_) if s.pos == s.start => {
                        File::create(&path).await?;
                    }
                    Err(_) => {
                        return Err(Error::NotResumable(format!(
                            "missing part file for segment {}",
                            s.id
                        )))
                    }
                }
            }
        }

        let sh = Arc::new(Shared {
            client,
            url: p.final_url.clone(),
            referer,
            total: p.total,
            etag: p.etag.clone(),
            last_modified: p.last_modified.clone(),
            resume_key: self.resume_key.clone(),
            user_agent: self.user_agent.clone(),
            cookie: self.cookie.clone(),
            output: output.clone(),
            parts_dir: parts_dir.clone(),
            connections: self.connections,
            retry_base: self.retry_base,
            retry_max: self.retry_max,
            limiter: self.speed_limit.map(Limiter::new),
            state: Mutex::new(st),
            notify: Notify::new(),
            downloaded: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            range_unsupported: AtomicBool::new(false),
            on_progress: self.on_progress.clone(),
            cancel: cancel.clone(),
            credentials: credentials.clone(),
        });

        {
            let st = sh.state.lock().await;
            let already: u64 = st.segments.iter().map(|s| s.pos - s.start).sum();
            sh.downloaded.store(already, Ordering::Relaxed);
            sh.persist(&st).await;
        }

        let progress_task = tokio::spawn({
            let sh = sh.clone();
            async move {
                let mut last_bytes = sh.downloaded.load(Ordering::Relaxed);
                let mut last_t = Instant::now();
                loop {
                    sleep(Duration::from_millis(500)).await;
                    if sh.finished.load(Ordering::Relaxed) {
                        break;
                    }
                    let now = Instant::now();
                    let bytes = sh.downloaded.load(Ordering::Relaxed);
                    let dt = now.duration_since(last_t).as_secs_f64().max(1e-9);
                    let speed = bytes.saturating_sub(last_bytes) as f64 / dt;
                    last_bytes = bytes;
                    last_t = now;
                    if let Some(cb) = &sh.on_progress {
                        cb(Progress {
                            downloaded: bytes,
                            total: sh.total,
                            speed_bps: speed,
                            connections: sh.connections,
                            output: sh.output.clone(),
                        });
                    }
                    let st = sh.state.lock().await;
                    sh.persist(&st).await;
                }
            }
        });

        let mut handles = Vec::with_capacity(sh.connections);
        for _ in 0..sh.connections {
            handles.push(tokio::spawn(worker(sh.clone())));
        }

        while !sh.all_done().await
            && !sh.range_unsupported.load(Ordering::Relaxed)
            && !sh.is_cancelled()
        {
            sleep(Duration::from_millis(200)).await;
        }
        sh.finished.store(true, Ordering::SeqCst);
        sh.notify.notify_waiters();
        for h in handles {
            let _ = h.await;
        }
        progress_task.abort();

        if sh.is_cancelled() {
            info!("download cancelled; {} part file(s) kept for resume", {
                let st = sh.state.lock().await;
                st.segments.len()
            });
            return Err(Error::Cancelled);
        }

        if sh.range_unsupported.load(Ordering::Relaxed) {
            warn!("server ignored Range after probe; falling back to a single stream");
            let _ = fs::remove_dir_all(&parts_dir).await;
            let stats = single_stream(
                &sh.client,
                &sh.url,
                &sh.output,
                &sh.user_agent,
                sh.referer.as_deref(),
                sh.cookie.as_deref(),
                sh.credentials.as_ref(),
                sh.total,
                self.speed_limit,
                cancel.clone(),
                sh.on_progress.clone(),
                started,
            )
            .await?;
            if let Some(expected) = &expected_sha256 {
                verify_sha256(&output, expected, cancel.as_ref()).await?;
            }
            return Ok(stats);
        }

        let segments = {
            let st = sh.state.lock().await;
            st.segments.len()
        };
        merge(&sh).await?;
        if let Some(expected) = &expected_sha256 {
            verify_sha256(&output, expected, cancel.as_ref()).await?;
        }

        Ok(Stats {
            total: sh.total,
            elapsed: started.elapsed(),
            segments,
            ranged: true,
            output,
        })
    }

    /// FTP path: single data connection with REST resume.
    async fn run_ftp(
        self,
        started: Instant,
        cancel: Option<Arc<AtomicBool>>,
        expected_sha256: Option<String>,
    ) -> Result<Stats> {
        let credentials = self.network.credentials.clone();
        let output = self.output.clone();
        let url = self.url.clone();
        let stats = crate::ftp::download(
            &url,
            &output,
            self.speed_limit,
            credentials.as_ref(),
            cancel.clone(),
            self.on_progress.clone(),
            started,
            self.resume,
        )
        .await?;
        if let Some(expected) = &expected_sha256 {
            verify_sha256(&output, expected, cancel.as_ref()).await?;
        }
        Ok(stats)
    }
}

struct SegState {
    segments: Vec<Segment>,
    next_id: u32,
}

struct Shared {
    client: reqwest::Client,
    url: String,
    referer: Option<String>,
    total: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    resume_key: Option<String>,
    user_agent: String,
    cookie: Option<String>,
    output: PathBuf,
    parts_dir: PathBuf,
    connections: usize,
    retry_base: Duration,
    retry_max: Duration,
    limiter: Option<Limiter>,
    state: Mutex<SegState>,
    notify: Notify,
    downloaded: AtomicU64,
    finished: AtomicBool,
    range_unsupported: AtomicBool,
    on_progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
    credentials: Option<Credentials>,
}

fn is_cancelled(flag: Option<&Arc<AtomicBool>>) -> bool {
    flag.is_some_and(|f| f.load(Ordering::Relaxed))
}

impl Shared {
    fn is_cancelled(&self) -> bool {
        is_cancelled(self.cancel.as_ref())
    }

    fn part_path(&self, id: u32) -> PathBuf {
        self.parts_dir.join(format!("part_{id:04}"))
    }

    async fn persist(&self, st: &SegState) {
        let m = Manifest {
            version: MANIFEST_VERSION,
            url: self.url.clone(),
            output: self.output.display().to_string(),
            total: self.total,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            connections: self.connections,
            segments: st.segments.clone(),
            resume_key: self.resume_key.clone(),
        };
        if let Ok(json) = serde_json::to_vec_pretty(&m) {
            let tmp = self.parts_dir.join("manifest.json.tmp");
            if fs::write(&tmp, json).await.is_ok() {
                let _ = fs::rename(&tmp, self.parts_dir.join("manifest.json")).await;
            }
        }
    }

    async fn all_done(&self) -> bool {
        let st = self.state.lock().await;
        st.segments.iter().all(|s| s.remaining() == 0)
    }

    /// Claim an unclaimed segment, or split the largest remaining one.
    async fn claim(&self) -> Option<u32> {
        let mut st = self.state.lock().await;
        if let Some(i) = st
            .segments
            .iter()
            .position(|s| !s.done && s.remaining() > 0 && !s.claimed)
        {
            st.segments[i].claimed = true;
            return Some(st.segments[i].id);
        }
        if split_largest(&mut st) {
            let i = st.segments.len() - 1;
            st.segments[i].claimed = true;
            let id = st.segments[i].id;
            self.persist(&st).await;
            return Some(id);
        }
        None
    }

    async fn segment_bounds(&self, id: u32) -> Option<(u64, u64, u64)> {
        let st = self.state.lock().await;
        st.segments
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.start, s.pos, s.end))
    }

    async fn set_pos(&self, id: u32, pos: u64) {
        let mut st = self.state.lock().await;
        if let Some(s) = st.segments.iter_mut().find(|s| s.id == id) {
            s.pos = pos;
        }
    }

    async fn mark_done(&self, id: u32) {
        let mut st = self.state.lock().await;
        if let Some(s) = st.segments.iter_mut().find(|s| s.id == id) {
            s.pos = s.end + 1;
            s.done = true;
        }
        self.persist(&st).await;
    }
}

/// Split the segment with the most remaining bytes, handing its tail to a new
/// worker.
///
/// IDM only splits once at least `MIN_SPLIT_BYTES` (46,721 = `0xb681`) remain,
/// then gives away half of what is left; the existing segment keeps the head,
/// the new one takes the tail.
fn split_largest(st: &mut SegState) -> bool {
    let mut best: Option<usize> = None;
    let mut best_rem = MIN_SPLIT_BYTES - 1;
    for (i, s) in st.segments.iter().enumerate() {
        if s.done {
            continue;
        }
        let rem = s.remaining();
        if rem >= MIN_SPLIT_BYTES && rem > best_rem {
            best_rem = rem;
            best = Some(i);
        }
    }
    let Some(i) = best else { return false };
    let seg = &st.segments[i];
    let start = seg.pos.max(seg.start);
    let end = seg.end;
    if end <= start {
        return false;
    }
    let remaining = end - start + 1;
    if remaining < MIN_SPLIT_BYTES {
        return false;
    }
    // Half of what is left goes to the tail; the existing segment keeps the head.
    let give = (remaining / 2).max(MIN_CHUNK_BYTES);
    let mid = end + 1 - give;
    if mid <= start || mid > end {
        return false;
    }
    st.segments[i].end = mid - 1;
    let id = st.next_id;
    st.next_id += 1;
    st.segments.push(Segment::new(id, mid, end));
    true
}

async fn worker(sh: Arc<Shared>) {
    loop {
        if sh.finished.load(Ordering::Relaxed) || sh.is_cancelled() {
            return;
        }
        match sh.claim().await {
            Some(id) => {
                if let Err(e) = download_segment(sh.clone(), id).await {
                    warn!("segment {id} aborted: {e}");
                    let mut st = sh.state.lock().await;
                    if let Some(s) = st.segments.iter_mut().find(|s| s.id == id) {
                        s.claimed = false;
                    }
                    drop(st);
                    sleep(sh.retry_base).await;
                }
            }
            None => {
                tokio::select! {
                    _ = sh.notify.notified() => {}
                    _ = sleep(Duration::from_millis(200)) => {}
                }
            }
        }
    }
}

async fn download_segment(sh: Arc<Shared>, id: u32) -> Result<()> {
    let mut attempt: u32 = 0;
    loop {
        if sh.finished.load(Ordering::Relaxed) {
            return Ok(());
        }
        if sh.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Some((start, pos, end)) = sh.segment_bounds(id).await else {
            return Ok(());
        };
        if pos > end {
            sh.mark_done(id).await;
            sh.notify.notify_waiters();
            return Ok(());
        }
        match stream_range(&sh, id, start, pos, end).await {
            Ok(()) => {
                attempt = 0;
            }
            Err(Error::RemoteChanged) => return Err(Error::RemoteChanged),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => {
                attempt = attempt.saturating_add(1);
                let mult = 1u32 << attempt.min(5);
                let backoff = (sh.retry_base * mult).min(sh.retry_max);
                warn!("segment {id} attempt {attempt} failed: {e}; retrying in {backoff:?}");
                sleep(backoff).await;
            }
        }
    }
}

async fn stream_range(
    sh: &Arc<Shared>,
    id: u32,
    seg_start: u64,
    mut pos: u64,
    end: u64,
) -> Result<()> {
    let mut req = sh
        .client
        .get(&sh.url)
        .header(USER_AGENT, sh.user_agent.as_str())
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, format!("bytes={pos}-{end}"));
    if let Some(c) = &sh.cookie {
        req = req.header(COOKIE, c.as_str());
    }
    if let Some(r) = &sh.referer {
        req = req.header(REFERER, r.as_str());
    }
    // Unlike IDM, validate that the remote file is still the same object.
    if let Some(etag) = &sh.etag {
        req = req.header(IF_RANGE, etag.as_str());
    }
    if let Some(auth) = &sh.credentials {
        req = req.basic_auth(&auth.username, Some(&auth.password));
    }

    let resp = req.send().await?;
    if let Some(error) = crate::net::auth_required(&resp, &sh.url) {
        return Err(error);
    }
    if resp.status() == StatusCode::OK {
        sh.range_unsupported.store(true, Ordering::SeqCst);
        sh.finished.store(true, Ordering::SeqCst);
        sh.notify.notify_waiters();
        return Ok(());
    }
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(Error::BadStatus(resp.status().as_u16()));
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(sh.part_path(id))
        .await?;
    file.seek(SeekFrom::Start(pos - seg_start)).await?;

    let mut stream = resp.bytes_stream();
    let mut last_persist = Instant::now();
    while let Some(item) = stream.next().await {
        if sh.is_cancelled() {
            file.flush().await?;
            let st = sh.state.lock().await;
            sh.persist(&st).await;
            return Err(Error::Cancelled);
        }
        let chunk = item?;
        let Some((_, _, cur_end)) = sh.segment_bounds(id).await else {
            break;
        };
        if pos > cur_end {
            break;
        }
        let room = cur_end - pos + 1;
        let n = (chunk.len() as u64).min(room) as usize;
        file.write_all(&chunk[..n]).await?;
        pos += n as u64;
        sh.downloaded.fetch_add(n as u64, Ordering::Relaxed);
        sh.set_pos(id, pos).await;
        if let Some(l) = &sh.limiter {
            l.acquire(n as u64).await;
        }
        if n < chunk.len() {
            break; // reached an end that another worker took over
        }
        if last_persist.elapsed() >= Duration::from_millis(500) {
            let st = sh.state.lock().await;
            sh.persist(&st).await;
            last_persist = Instant::now();
        }
    }
    file.flush().await?;
    Ok(())
}

async fn merge(sh: &Arc<Shared>) -> Result<()> {
    let segments = {
        let st = sh.state.lock().await;
        let mut segs = st.segments.clone();
        segs.sort_by_key(|s| s.start);
        segs
    };

    let mut out = File::create(&sh.output).await?;
    for s in &segments {
        if sh.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let part_path = sh.part_path(s.id);
        let md = fs::metadata(&part_path).await?;
        let expected = s.end - s.start + 1;
        if md.len() < expected {
            return Err(Error::Other(format!(
                "segment {} has {} bytes, expected {expected}",
                s.id,
                md.len()
            )));
        }
        if md.len() > expected {
            // A worker may have written a few bytes past a segment end that was
            // reduced by a concurrent split; the tail lives in its own part file.
            let f = OpenOptions::new().write(true).open(&part_path).await?;
            f.set_len(expected).await?;
        }
        let mut part = File::open(&part_path).await?;
        tokio::io::copy(&mut part, &mut out).await?;
    }
    out.flush().await?;
    out.sync_all().await?;
    let len = out.metadata().await?.len();
    if len != sh.total {
        return Err(Error::Other(format!(
            "merged file is {len} bytes, expected {}",
            sh.total
        )));
    }
    drop(out);
    let _ = fs::remove_dir_all(&sh.parts_dir).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn single_stream(
    client: &reqwest::Client,
    url: &str,
    output: &Path,
    user_agent: &str,
    referer: Option<&str>,
    cookie: Option<&str>,
    credentials: Option<&Credentials>,
    total_hint: u64,
    speed_limit: Option<u64>,
    cancel: Option<Arc<AtomicBool>>,
    on_progress: Option<ProgressFn>,
    started: Instant,
) -> Result<Stats> {
    let resp = client
        .get(url)
        .header(USER_AGENT, user_agent)
        .header(ACCEPT_ENCODING, "identity");
    let resp = match referer {
        Some(referer) => resp.header(REFERER, referer),
        None => resp,
    };
    let resp = match cookie {
        Some(cookie) => resp.header(COOKIE, cookie),
        None => resp,
    };
    let resp = match credentials {
        Some(auth) => resp.basic_auth(&auth.username, Some(&auth.password)),
        None => resp,
    };
    let resp = resp.send().await?;
    if let Some(error) = crate::net::auth_required(&resp, url) {
        return Err(error);
    }
    if !resp.status().is_success() {
        return Err(Error::BadStatus(resp.status().as_u16()));
    }
    let total = if total_hint == 0 {
        resp.content_length().unwrap_or(0)
    } else {
        total_hint
    };

    let mut tmp_name = output.as_os_str().to_os_string();
    tmp_name.push(".rgtmp");
    let tmp = PathBuf::from(tmp_name);

    let mut f = File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();
    let limiter = speed_limit.map(Limiter::new);
    let mut done: u64 = 0;
    let mut last_t = Instant::now();
    let mut last_bytes: u64 = 0;

    while let Some(item) = stream.next().await {
        if is_cancelled(cancel.as_ref()) {
            f.flush().await?;
            return Err(Error::Cancelled);
        }
        let chunk = item?;
        f.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if let Some(l) = &limiter {
            l.acquire(chunk.len() as u64).await;
        }
        if let Some(cb) = &on_progress {
            if last_t.elapsed() >= Duration::from_millis(500) {
                let dt = last_t.elapsed().as_secs_f64().max(1e-9);
                cb(Progress {
                    downloaded: done,
                    total,
                    speed_bps: done.saturating_sub(last_bytes) as f64 / dt,
                    connections: 1,
                    output: output.to_path_buf(),
                });
                last_t = Instant::now();
                last_bytes = done;
            }
        }
    }
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    fs::rename(&tmp, output).await?;

    Ok(Stats {
        total: done,
        elapsed: started.elapsed(),
        segments: 1,
        ranged: false,
        output: output.to_path_buf(),
    })
}

/// Stream the finished file through SHA-256 and compare with the expected hex
/// digest. Deliberate improvement over IDM, which never validates content.
async fn verify_sha256(
    path: &Path,
    expected: &str,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<()> {
    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if is_cancelled(cancel) {
            return Err(Error::Cancelled);
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = format!("{:x}", hasher.finalize());
    if !got.eq_ignore_ascii_case(expected.trim()) {
        return Err(Error::Other(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected.trim(),
            got
        )));
    }
    Ok(())
}

fn referer_for(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let mut origin = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        origin.push_str(&format!(":{port}"));
    }
    origin.push('/');
    Some(origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_largest_at_midpoint() {
        let mut st = SegState {
            segments: vec![Segment::new(0, 0, 1_000_000)],
            next_id: 1,
        };
        assert!(split_largest(&mut st));
        assert_eq!(st.segments.len(), 2);
        // 1,000,001 bytes remain; the tail takes 500,000 of them.
        assert_eq!(st.segments[0].end, 500_000);
        assert_eq!(st.segments[1].start, 500_001);
        assert_eq!(st.segments[1].end, 1_000_000);
    }

    #[test]
    fn does_not_split_below_threshold() {
        let mut st = SegState {
            segments: vec![Segment::new(0, 0, 1_000)],
            next_id: 1,
        };
        assert!(!split_largest(&mut st));
    }

    #[test]
    fn splits_small_ranges_like_idm() {
        // 46,721 bytes is IDM's 0xb681 adaptive threshold; both halves stay
        // above the 11,680-byte (0x2da0) floor.
        let mut st = SegState {
            segments: vec![Segment::new(0, 0, 46_720)],
            next_id: 1,
        };
        assert!(split_largest(&mut st));
        let tail = &st.segments[1];
        assert_eq!(tail.end, 46_720);
        assert_eq!(tail.end - tail.start + 1, 23_360);
        // Anything smaller than two minimum chunks is not worth splitting.
        let mut tiny = SegState {
            segments: vec![Segment::new(0, 0, 23_359)],
            next_id: 1,
        };
        assert!(!split_largest(&mut tiny));
    }

    #[test]
    fn split_uses_current_position() {
        let mut st = SegState {
            segments: vec![Segment::new(0, 0, 1_000_000)],
            next_id: 1,
        };
        st.segments[0].pos = 400_000;
        assert!(split_largest(&mut st));
        // 600,001 bytes remain from the current position; the tail takes 300,000.
        assert_eq!(st.segments[0].end, 700_000);
        assert_eq!(st.segments[1].start, 700_001);
    }

    #[test]
    fn takeover_splits_largest_remaining_segment() {
        let mut first = Segment::new(0, 0, 2_000_000);
        first.pos = 1_900_000;
        first.claimed = true;
        let mut second = Segment::new(1, 2_000_001, 3_000_000);
        second.claimed = true;
        let mut st = SegState {
            segments: vec![first, second],
            next_id: 2,
        };

        assert!(split_largest(&mut st));
        assert_eq!(st.segments.len(), 3);
        // Segment 1 has more remaining bytes (1,000,000) than segment 0
        // (100,001), so takeover must split segment 1.
        assert_eq!(st.segments[1].end, 2_500_000);
        assert_eq!(st.segments[2].start, 2_500_001);
        assert!(st.segments[0].claimed);
        assert!(st.segments[1].claimed);
    }

    #[test]
    fn takeover_can_split_a_claimed_segment() {
        let mut segment = Segment::new(7, 100, 1_000_099);
        segment.pos = 400_100;
        segment.claimed = true;
        let mut st = SegState {
            segments: vec![segment],
            next_id: 8,
        };

        assert!(split_largest(&mut st));
        assert_eq!(st.segments[0].end, 700_099);
        assert_eq!(st.segments[1].start, 700_100);
        assert_eq!(st.segments[1].id, 8);
        assert!(!st.segments[1].claimed);
    }

    #[test]
    fn completed_segments_are_never_taken_over() {
        let mut done = Segment::new(0, 0, 2_000_000);
        done.pos = done.end + 1;
        done.done = true;
        let active = Segment::new(1, 2_000_001, 3_000_000);
        let mut st = SegState {
            segments: vec![done, active],
            next_id: 2,
        };

        assert!(split_largest(&mut st));
        assert_eq!(st.segments[0].end, 2_000_000);
        assert_eq!(st.segments[1].end, 2_500_000);
        assert_eq!(st.segments[2].start, 2_500_001);
    }

    #[test]
    fn tail_never_drops_below_minimum_chunk() {
        // The smallest splittable range hands the tail half of what is left,
        // which still clears IDM's 11,680-byte (0x2da0) floor.
        let mut st = SegState {
            segments: vec![Segment::new(0, 0, MIN_SPLIT_BYTES - 1)],
            next_id: 1,
        };
        assert!(split_largest(&mut st));
        let tail = &st.segments[1];
        assert!(tail.end - tail.start + 1 >= MIN_CHUNK_BYTES);
    }

    #[test]
    fn remaining_is_zero_when_done() {
        let mut s = Segment::new(0, 0, 99);
        assert_eq!(s.remaining(), 100);
        s.pos = 100;
        assert_eq!(s.remaining(), 0);
        s.done = true;
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn referer_uses_origin() {
        assert_eq!(
            referer_for("https://example.com:8443/path/file?q=1").as_deref(),
            Some("https://example.com:8443/")
        );
    }
}
