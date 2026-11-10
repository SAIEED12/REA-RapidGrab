//! Breadth-first site crawler used by the Site Grabber page.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::header::{
    ACCEPT, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, REFERER, USER_AGENT,
};
use reqwest::Url;

use crate::extract::extract_urls;

/// Number of pages fetched concurrently.
const CONCURRENCY: usize = 4;

/// Refuse to buffer pages larger than this; huge pages are almost always
/// accidental (a mislabelled video served as text/html).
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Extensions that make a URL a page to crawl rather than a file to grab.
const LANDING_EXTS: [&str; 10] = [
    "html", "htm", "xhtml", "shtml", "php", "asp", "aspx", "jsp", "cgi", "cfm",
];

/// Named extension groups offered by the grabber UI.
pub const PRESETS: &[(&str, &[&str])] = &[
    (
        "Video",
        &[
            "mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "wmv", "mpg", "mpeg", "3gp",
            "ogv", "ts",
        ],
    ),
    (
        "Audio",
        &["mp3", "m4a", "aac", "wav", "ogg", "flac", "wma", "opus"],
    ),
    (
        "Images",
        &["jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "tiff", "ico"],
    ),
    (
        "Archives",
        &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "img"],
    ),
    (
        "Documents",
        &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "odt", "csv",
            "md", "epub",
        ],
    ),
    (
        "Programs",
        &["exe", "msi", "apk", "dmg", "pkg", "deb", "rpm", "jar"],
    ),
];

/// Preset names in display order.
pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|(name, _)| *name).collect()
}

/// Build a de-duplicated extension list from preset names plus free-form
/// entries (dots are tolerated: `.MP4` and `mp4` are the same).
pub fn extensions_from(presets: &[String], custom: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |ext: &str| {
        let ext = normalize_ext(ext);
        if !ext.is_empty() && !out.iter().any(|e| e == &ext) {
            out.push(ext);
        }
    };
    for (name, exts) in PRESETS {
        if presets.iter().any(|p| p.eq_ignore_ascii_case(name)) {
            for ext in *exts {
                push(ext);
            }
        }
    }
    for ext in custom {
        push(ext);
    }
    out
}

fn normalize_ext(ext: &str) -> String {
    ext.trim()
        .trim_start_matches('.')
        .trim_start_matches('*')
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

/// What a discovered link is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Matches a selected extension: report it, do not fetch it.
    File,
    /// Looks like a page: crawl it when the depth budget allows.
    Page,
}

/// One downloadable file discovered on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabFile {
    pub url: String,
    /// Page the link was found on (used as `Referer` when downloading).
    pub page_url: String,
    pub filename: String,
    pub extension: String,
}

/// Progress events emitted by [`scan`].
#[derive(Debug, Clone)]
pub enum GrabEvent {
    Page {
        url: String,
        depth: usize,
        /// Number of links to further pages found on it.
        links: usize,
    },
    Found(GrabFile),
    Failed {
        url: String,
        error: String,
    },
    Done(GrabReport),
}

/// Counters for a finished scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrabReport {
    pub pages: usize,
    pub files: usize,
    pub errors: usize,
    pub cancelled: bool,
}

/// Crawl limits and filters.
#[derive(Debug, Clone)]
pub struct GrabRules {
    /// Only follow links whose host equals the start page's host.
    pub same_domain: bool,
    /// 0 fetches only the start page.
    pub max_depth: usize,
    pub max_pages: usize,
    /// Lower-case extensions without the dot.
    pub extensions: Vec<String>,
    /// Substring or `*` wildcard patterns a file URL must match (empty = all).
    pub include: Vec<String>,
    /// Substring or `*` wildcard patterns that reject a file URL.
    pub exclude: Vec<String>,
    pub respect_robots: bool,
    /// Delay before each page request (politeness, not per-host).
    pub delay: Duration,
}

impl Default for GrabRules {
    fn default() -> Self {
        Self {
            same_domain: true,
            max_depth: 2,
            max_pages: 200,
            extensions: extensions_from(
                &["Video", "Audio", "Archives", "Documents", "Programs"]
                    .map(str::to_string),
                &[],
            ),
            include: Vec::new(),
            exclude: Vec::new(),
            respect_robots: true,
            delay: Duration::from_millis(120),
        }
    }
}

impl GrabRules {
    /// True when `url` passes the include/exclude filters.
    pub fn allows_file(&self, url: &str) -> bool {
        let lower = url.to_ascii_lowercase();
        if !self.include.is_empty()
            && !self
                .include
                .iter()
                .any(|pattern| pattern_matches(&lower, &pattern.to_ascii_lowercase()))
        {
            return false;
        }
        !self
            .exclude
            .iter()
            .any(|pattern| pattern_matches(&lower, &pattern.to_ascii_lowercase()))
    }

    fn selects(&self, extension: &str) -> bool {
        self.extensions.iter().any(|e| e == extension)
    }
}

/// Network options shared by every request of a scan.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub network: rg_core::NetworkConfig,
    /// Overrides the reqwest default when set.
    pub user_agent: Option<String>,
    pub cookie: Option<String>,
}

struct PageBody {
    final_url: Url,
    /// `None` when the response is not HTML (the URL turned out to be a file).
    html: Option<String>,
}

struct PageFetch {
    url: Url,
    depth: usize,
    referer: Option<String>,
    outcome: Result<PageBody, String>,
}

/// Crawls `start`, invoking `on_event` synchronously as results arrive.
///
/// `cancel` is polled between requests; setting it stops scheduling new pages
/// and ends the scan with [`GrabReport::cancelled`] set.
pub async fn scan<F>(
    start: &str,
    rules: &GrabRules,
    options: &ScanOptions,
    cancel: Arc<AtomicBool>,
    on_event: &mut F,
) -> GrabReport
where
    F: FnMut(GrabEvent),
{
    let mut report = GrabReport::default();

    let start_url = match Url::parse(start.trim()) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => url,
        _ => {
            report.errors = 1;
            on_event(GrabEvent::Failed {
                url: start.to_string(),
                error: "enter a valid http(s) URL".to_string(),
            });
            on_event(GrabEvent::Done(report.clone()));
            return report;
        }
    };
    let start_host = start_url.host_str().map(str::to_string);

    let client = match options.network.client_for(start_url.as_str(), CONCURRENCY).await {
        Ok(client) => client,
        Err(error) => {
            report.errors = 1;
            on_event(GrabEvent::Failed {
                url: start_url.to_string(),
                error: format!("could not create HTTP client: {error}"),
            });
            on_event(GrabEvent::Done(report.clone()));
            return report;
        }
    };

    let robots = if rules.respect_robots {
        fetch_robots(&client, &start_url, options).await
    } else {
        RobotsRules::default()
    };

    let max_depth = rules.max_depth;
    let max_pages = rules.max_pages.max(1);
    let mut queue: VecDeque<(Url, usize, Option<String>)> = VecDeque::new();
    queue.push_back((start_url.clone(), 0, None));
    let mut visited: HashSet<String> = HashSet::new();
    let mut found: HashSet<String> = HashSet::new();
    let mut in_flight: FuturesUnordered<BoxFuture<'static, PageFetch>> = FuturesUnordered::new();
    let mut started = 0usize;

    loop {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }

        // Keep up to CONCURRENCY fetches running (and never exceed max_pages).
        while in_flight.len() < CONCURRENCY && started < max_pages {
            let Some((url, depth, referer)) = queue.pop_front() else {
                break;
            };
            if !visited.insert(url_key(&url)) {
                continue;
            }
            started += 1;
            in_flight.push(fetch_page(
                client.clone(),
                url,
                depth,
                referer,
                options.user_agent.clone(),
                options.cookie.clone(),
                rules.delay,
            ));
        }

        let Some(fetch) = in_flight.next().await else {
            break;
        };

        match fetch.outcome {
            Err(error) => {
                report.errors += 1;
                on_event(GrabEvent::Failed {
                    url: fetch.url.to_string(),
                    error,
                });
            }
            Ok(body) => {
                report.pages += 1;
                match body.html {
                    Some(html) => {
                        let base = body.final_url.clone();
                        let mut links = 0usize;
                        for raw in extract_urls(&html) {
                            let Some(candidate) = resolve_url(&base, &raw) else {
                                continue;
                            };
                            match classify(&candidate, rules) {
                                Some(LinkKind::File) => {
                                    let key = url_key(&candidate);
                                    if !found.insert(key) {
                                        continue;
                                    }
                                    // Files can live on a CDN, so same-domain
                                    // only constrains page traversal.
                                    report.files += 1;
                                    on_event(GrabEvent::Found(GrabFile {
                                        url: candidate.to_string(),
                                        page_url: base.to_string(),
                                        filename: filename_of(&candidate),
                                        extension: extension_of(&candidate),
                                    }));
                                }
                                Some(LinkKind::Page) => {
                                    links += 1;
                                    if fetch.depth < max_depth
                                        && started + queue.len() < max_pages
                                        && same_site(&candidate, &start_host, rules.same_domain)
                                        && robots.allowed(candidate.path())
                                    {
                                        queue.push_back((
                                            candidate,
                                            fetch.depth + 1,
                                            Some(base.to_string()),
                                        ));
                                    }
                                }
                                None => {}
                            }
                        }
                        on_event(GrabEvent::Page {
                            url: base.to_string(),
                            depth: fetch.depth,
                            links,
                        });
                    }
                    None => {
                        // A URL that looked like a page served a binary.
                        let key = url_key(&body.final_url);
                        if found.insert(key) {
                            report.files += 1;
                            on_event(GrabEvent::Found(GrabFile {
                                url: body.final_url.to_string(),
                                page_url: fetch.referer.clone().unwrap_or_default(),
                                filename: filename_of(&body.final_url),
                                extension: extension_of(&body.final_url),
                            }));
                        }
                        on_event(GrabEvent::Page {
                            url: body.final_url.to_string(),
                            depth: fetch.depth,
                            links: 0,
                        });
                    }
                }
            }
        }
    }

    // Drain nothing else: dropping in_flight cancels outstanding requests.
    on_event(GrabEvent::Done(report.clone()));
    report
}

fn fetch_page(
    client: reqwest::Client,
    url: Url,
    depth: usize,
    referer: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    delay: Duration,
) -> BoxFuture<'static, PageFetch> {
    Box::pin(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let mut request = client.get(url.clone()).header(
            ACCEPT,
            "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
        );
        if let Some(ua) = user_agent {
            request = request.header(USER_AGENT, ua);
        }
        if let Some(cookie) = cookie {
            request = request.header(COOKIE, cookie);
        }
        if let Some(referer) = referer.clone() {
            request = request.header(REFERER, referer);
        }

        let outcome = async {
            let response = request.send().await.map_err(|e| e.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            let final_url = response.url().clone();
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.to_ascii_lowercase());
            let disposition = response
                .headers()
                .get(CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.to_ascii_lowercase());
            let is_html = match content_type.as_deref() {
                Some(value) => {
                    value.contains("text/html") || value.contains("application/xhtml")
                }
                None => true,
            };
            let is_attachment = disposition
                .as_deref()
                .is_some_and(|value| value.starts_with("attachment"));
            if !is_html || is_attachment {
                return Ok(PageBody {
                    final_url,
                    html: None,
                });
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_PAGE_BYTES)
            {
                return Err(format!("page larger than {} MB", MAX_PAGE_BYTES / 1024 / 1024));
            }
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;
            if bytes.len() as u64 > MAX_PAGE_BYTES {
                return Err(format!("page larger than {} MB", MAX_PAGE_BYTES / 1024 / 1024));
            }
            Ok(PageBody {
                final_url,
                html: Some(String::from_utf8_lossy(&bytes).into_owned()),
            })
        }
        .await;

        PageFetch {
            url,
            depth,
            referer,
            outcome,
        }
    })
}

async fn fetch_robots(
    client: &reqwest::Client,
    start: &Url,
    options: &ScanOptions,
) -> RobotsRules {
    let Ok(url) = start.join("/robots.txt") else {
        return RobotsRules::default();
    };
    let mut request = client.get(url);
    if let Some(ua) = &options.user_agent {
        request = request.header(USER_AGENT, ua.clone());
    }
    let Ok(response) = request.send().await else {
        return RobotsRules::default();
    };
    if !response.status().is_success() {
        return RobotsRules::default();
    }
    match response.bytes().await {
        Ok(bytes) => RobotsRules::parse(&String::from_utf8_lossy(&bytes)),
        Err(_) => RobotsRules::default(),
    }
}

/// Parsed `robots.txt` rules for the `*` user agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RobotsRules {
    disallow: Vec<String>,
    allow: Vec<String>,
}

impl RobotsRules {
    pub fn parse(text: &str) -> Self {
        let mut rules = Self::default();
        let mut applies = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => applies = value == "*",
                "disallow" if applies && !value.is_empty() => {
                    rules.disallow.push(value.to_string())
                }
                "allow" if applies && !value.is_empty() => rules.allow.push(value.to_string()),
                _ => {}
            }
        }
        rules
    }

    /// Longest-prefix match, with `Allow` winning ties (the usual convention).
    pub fn allowed(&self, path: &str) -> bool {
        let path = if path.is_empty() { "/" } else { path };
        let longest = |rules: &[String]| {
            rules
                .iter()
                .filter(|prefix| path.starts_with(prefix.as_str()))
                .map(|prefix| prefix.len())
                .max()
        };
        match (longest(&self.disallow), longest(&self.allow)) {
            (Some(disallow), Some(allow)) => allow >= disallow,
            (Some(_), None) => false,
            _ => true,
        }
    }
}

/// Resolve `raw` against `base`, dropping fragments and non-network schemes.
fn resolve_url(base: &Url, raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if ["javascript:", "mailto:", "tel:", "data:", "blob:", "about:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return None;
    }
    let mut url = base.join(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

fn classify(url: &Url, rules: &GrabRules) -> Option<LinkKind> {
    let extension = extension_of(url);
    if !extension.is_empty() && rules.selects(&extension) {
        return rules
            .allows_file(url.as_str())
            .then_some(LinkKind::File);
    }
    if extension.is_empty() || LANDING_EXTS.contains(&extension.as_str()) {
        return Some(LinkKind::Page);
    }
    // Some other asset (.css, .js, .jpg when images are not selected, …).
    None
}

fn same_site(candidate: &Url, start_host: &Option<String>, same_domain: bool) -> bool {
    if !same_domain {
        return true;
    }
    match (candidate.host_str(), start_host.as_deref()) {
        (Some(host), Some(start)) => host.eq_ignore_ascii_case(start),
        _ => false,
    }
}

fn extension_of(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|segments| segments.filter(|s| !s.is_empty()).next_back())
        .unwrap_or("");
    match last.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            extension.to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

fn filename_of(url: &Url) -> String {
    let name = url
        .path_segments()
        .and_then(|segments| segments.filter(|s| !s.is_empty()).next_back())
        .unwrap_or("");
    if name.is_empty() {
        return "download.bin".to_string();
    }
    // Decode `%20`-style escapes, then strip characters Windows rejects so the
    // name is safe to join onto a directory.
    let decoded = percent_decode(name);
    rg_core::sanitize_filename(&decoded).unwrap_or_else(|| "download.bin".to_string())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let digit = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (digit(bytes[index + 1]), digit(bytes[index + 2])) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn url_key(url: &Url) -> String {
    let mut copy = url.clone();
    copy.set_fragment(None);
    copy.to_string()
}

/// Case-insensitive substring match, or `*` glob when the pattern contains it.
fn pattern_matches(text: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains('*') {
        return text.contains(pattern);
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut remaining = text;
    if let Some(first) = parts.first().filter(|part| !part.is_empty()) {
        match remaining.strip_prefix(first) {
            Some(rest) => remaining = rest,
            None => return false,
        }
    }
    let middle = parts.len().saturating_sub(2);
    for part in parts.iter().skip(1).take(middle).filter(|part| !part.is_empty()) {
        match remaining.find(part) {
            Some(index) => remaining = &remaining[index + part.len()..],
            None => return false,
        }
    }
    match parts.last() {
        Some(last) if parts.len() > 1 && !last.is_empty() => remaining.ends_with(last),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    type Route = (String, String, String);

    /// Minimal HTTP/1.1 server: `(path, content-type, body)` per route.
    async fn spawn_server(routes: Vec<(&'static str, &'static str, &'static str)>) -> String {
        serve_with(|_| {
            routes
                .into_iter()
                .map(|(path, content_type, body)| {
                    (path.to_string(), content_type.to_string(), body.to_string())
                })
                .collect()
        })
        .await
    }

    /// Binds first so `build` knows the ephemeral base URL (used for absolute
    /// cross-host links).
    async fn serve_with<F>(build: F) -> String
    where
        F: FnOnce(&str) -> Vec<Route>,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let routes: Arc<HashMap<String, (String, String)>> = Arc::new(
            build(&base)
                .into_iter()
                .map(|(path, content_type, body)| (path, (content_type, body)))
                .collect(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8192];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let path = path.split('?').next().unwrap_or(path).to_string();
                    let (status, (content_type, body)) = match routes.get(&path) {
                        Some(route) => ("200 OK", route.clone()),
                        None => (
                            "404 Not Found",
                            ("text/plain".to_string(), "not found".to_string()),
                        ),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        base
    }

    fn rules(extensions: &[&str]) -> GrabRules {
        GrabRules {
            extensions: extensions.iter().map(|e| e.to_string()).collect(),
            delay: Duration::ZERO,
            respect_robots: false,
            ..GrabRules::default()
        }
    }

    async fn run(base: &str, rules: &GrabRules) -> (GrabReport, Vec<GrabEvent>) {
        let mut events = Vec::new();
        let report = scan(
            base,
            rules,
            &ScanOptions::default(),
            Arc::new(AtomicBool::new(false)),
            &mut |event| events.push(event),
        )
        .await;
        (report, events)
    }

    fn found_urls(events: &[GrabEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                GrabEvent::Found(file) => Some(file.url.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn follows_relative_links_and_finds_files() {
        let base = spawn_server(vec![
            (
                "/",
                "text/html",
                r#"<a href="/a.html">a</a><a href="/movie.mp4">v</a>"#,
            ),
            (
                "/a.html",
                "text/html",
                r#"<a href="b.html">b</a><a href="/sub/clip.mkv">v</a>"#,
            ),
            ("/b.html", "text/html", "<p>end</p>"),
            ("/sub/clip.mkv", "video/x-matroska", "binary"),
        ])
        .await;
        let rules = rules(&["mp4", "mkv"]);
        let (report, events) = run(&format!("{base}/"), &rules).await;

        assert_eq!(report.pages, 3);
        let mut urls = found_urls(&events);
        urls.sort();
        assert_eq!(
            urls,
            vec![format!("{base}/movie.mp4"), format!("{base}/sub/clip.mkv")]
        );
        // Depth 2 is the default, so /b.html must have been fetched.
        assert!(events.iter().any(
            |e| matches!(e, GrabEvent::Page { url, .. } if url.ends_with("/b.html"))
        ));
    }

    #[tokio::test]
    async fn respects_depth_limit() {
        let base = spawn_server(vec![
            ("/", "text/html", r#"<a href="/one">1</a>"#),
            ("/one", "text/html", r#"<a href="/two">2</a>"#),
            ("/two", "text/html", "<p>deep</p>"),
        ])
        .await;
        let mut rules = rules(&["mp4"]);
        rules.max_depth = 1;
        let (report, events) = run(&format!("{base}/"), &rules).await;

        assert_eq!(report.pages, 2);
        assert!(!events
            .iter()
            .any(|e| matches!(e, GrabEvent::Page { url, .. } if url.ends_with("/two"))));
    }

    #[tokio::test]
    async fn stops_at_page_cap() {
        let base = serve_with(|_| {
            let mut routes = vec![(
                "/".to_string(),
                "text/html".to_string(),
                (0..10)
                    .map(|index| format!(r#"<a href="/p{index}.html">x</a>"#))
                    .collect::<String>(),
            )];
            for index in 0..10 {
                routes.push((
                    format!("/p{index}.html"),
                    "text/html".to_string(),
                    "<p>x</p>".to_string(),
                ));
            }
            routes
        })
        .await;
        let mut rules = rules(&["mp4"]);
        rules.max_pages = 3;
        rules.max_depth = 1;
        let (report, _) = run(&format!("{base}/"), &rules).await;

        assert_eq!(report.pages, 3);
    }

    #[tokio::test]
    async fn filters_by_extension_and_pattern() {
        let base = spawn_server(vec![(
            "/",
            "text/html",
            r#"<a href="/a.mp4">a</a><a href="/b.zip">b</a><a href="/c.pdf">c</a>"#,
        )])
        .await;
        let mut rules = rules(&["mp4", "zip", "pdf"]);
        rules.exclude = vec!["*b.zip".to_string()];
        let (report, events) = run(&format!("{base}/"), &rules).await;

        assert_eq!(report.files, 2);
        let urls = found_urls(&events);
        assert!(urls.contains(&format!("{base}/a.mp4")));
        assert!(urls.contains(&format!("{base}/c.pdf")));
        assert!(!urls.contains(&format!("{base}/b.zip")));
    }

    #[tokio::test]
    async fn same_domain_blocks_cross_host_pages_but_not_files() {
        let base = serve_with(|base| {
            let port = base.rsplit(':').next().unwrap();
            vec![
                (
                    "/".to_string(),
                    "text/html".to_string(),
                    format!(
                        r#"<a href="http://localhost:{port}/other">o</a><a href="http://localhost:{port}/x.mp4">f</a>"#
                    ),
                ),
                (
                    "/x.mp4".to_string(),
                    "video/mp4".to_string(),
                    "binary".to_string(),
                ),
                (
                    "/other".to_string(),
                    "text/html".to_string(),
                    "<p>other host</p>".to_string(),
                ),
            ]
        })
        .await;

        let mut rules = rules(&["mp4"]);
        rules.same_domain = true;
        let (report, events) = run(&format!("{base}/"), &rules).await;

        assert_eq!(report.pages, 1, "cross-host page must not be fetched");
        let urls = found_urls(&events);
        assert!(urls.iter().any(|url| url.ends_with("/x.mp4")));
    }

    #[tokio::test]
    async fn honours_robots_disallow() {
        let base = spawn_server(vec![
            (
                "/robots.txt",
                "text/plain",
                "User-agent: *\nDisallow: /private\n",
            ),
            (
                "/",
                "text/html",
                r#"<a href="/private/secret.html">s</a><a href="/public.html">p</a>"#,
            ),
            ("/public.html", "text/html", "<p>public</p>"),
            ("/private/secret.html", "text/html", "<p>secret</p>"),
        ])
        .await;
        let mut rules = rules(&["mp4"]);
        rules.respect_robots = true;
        rules.max_depth = 1;
        let (report, events) = run(&format!("{base}/"), &rules).await;

        assert_eq!(report.pages, 2);
        assert!(!events
            .iter()
            .any(|e| matches!(e, GrabEvent::Page { url, .. } if url.contains("/private/"))));
    }

    #[tokio::test]
    async fn cancel_stops_the_scan() {
        let base = spawn_server(vec![("/", "text/html", r#"<a href="/a.html">a</a>"#)])
            .await;
        let cancel = Arc::new(AtomicBool::new(true));
        let mut events = Vec::new();
        let report = scan(
            &format!("{base}/"),
            &rules(&["mp4"]),
            &ScanOptions::default(),
            cancel,
            &mut |event| events.push(event),
        )
        .await;
        assert!(report.cancelled);
        assert_eq!(report.pages, 0);
    }

    #[test]
    fn pattern_matching() {
        assert!(pattern_matches("https://ex.com/a.mp4", "*.mp4"));
        assert!(pattern_matches("https://ex.com/a.mp4", "*ex.com*"));
        assert!(pattern_matches("https://ex.com/a.mp4", "a.mp4"));
        assert!(!pattern_matches("https://ex.com/a.zip", "*.mp4"));
        assert!(!pattern_matches("https://ex.com/a.mp4", "*.zip"));
        assert!(pattern_matches("anything", "*"));
    }

    #[test]
    fn robots_rules() {
        let rules = RobotsRules::parse(
            "User-agent: bot\nDisallow: /\n\nUser-agent: *\nDisallow: /private\nAllow: /private/ok\n",
        );
        assert!(!rules.allowed("/private/secret"));
        assert!(rules.allowed("/private/ok/page"));
        assert!(rules.allowed("/public"));
    }

    #[test]
    fn filename_decodes_percent_escapes() {
        let url = Url::parse("https://ex.com/a/My%20Clip%2F1.mp4").unwrap();
        assert_eq!(filename_of(&url), "My Clip 1.mp4");
        let root = Url::parse("https://ex.com/").unwrap();
        assert_eq!(filename_of(&root), "download.bin");
    }

    #[test]
    fn extension_helpers() {
        let exts = extensions_from(
            &["video".to_string(), "Video".to_string()],
            &[".MP4".to_string(), "mkv".to_string()],
        );
        assert_eq!(exts, vec!["mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "wmv", "mpg", "mpeg", "3gp", "ogv", "ts"]);
        let url = Url::parse("https://ex.com/a/B.MP4?x=1").unwrap();
        assert_eq!(extension_of(&url), "mp4");
        assert_eq!(filename_of(&url), "B.MP4");
    }
}
