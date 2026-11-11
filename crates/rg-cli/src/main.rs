use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rg_core::{Download, NetworkConfig, Progress, ProxyConfig, ProxyMode};
use rg_resolve::{MediaEvent, MediaRequest, MediaStage, Tools};

#[derive(Parser)]
#[command(
    name = "rapidgrab",
    version,
    about = "Multi-segment resumable HTTP downloader"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL (resumes automatically when parts exist)
    Get {
        url: String,
        /// Output file path (derived from title/URL when omitted)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Directory for the derived output name (defaults to Downloads)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Parallel connections
        #[arg(short = 'n', long, default_value_t = rg_core::DEFAULT_CONNECTIONS)]
        connections: usize,
        /// Aggregate speed limit in bytes/sec (0 = unlimited)
        #[arg(long, default_value_t = 0)]
        limit: u64,
        /// Ignore existing parts and start over
        #[arg(long)]
        restart: bool,
        /// Verify the finished file against this SHA-256 (hex)
        #[arg(long)]
        sha256: Option<String>,
        /// Override the User-Agent header
        #[arg(long)]
        user_agent: Option<String>,
        /// Referer header (useful for protected downloads)
        #[arg(long)]
        referer: Option<String>,
        /// Cookie header value
        #[arg(long)]
        cookie: Option<String>,
        /// Page/video title used to derive the output name when --output is omitted
        #[arg(long)]
        title: Option<String>,
        /// Manual proxy endpoint (`host:port`, `http://â€¦`, `socks5://â€¦`)
        #[arg(long, value_name = "URL")]
        proxy: Option<String>,
        /// Proxy username (Basic auth)
        #[arg(long, value_name = "USER")]
        proxy_user: Option<String>,
        /// Proxy password (Basic auth)
        #[arg(long, value_name = "PASS")]
        proxy_pass: Option<String>,
        /// HTTP Basic username for the download itself
        #[arg(long, value_name = "USER")]
        user: Option<String>,
        /// HTTP Basic password for the download itself
        #[arg(long, value_name = "PASS")]
        password: Option<String>,
        /// Use the Windows system proxy (WinHTTP, including PAC/WPAD)
        #[arg(long)]
        system_proxy: bool,
        /// Explicit PAC URL used with --system-proxy
        #[arg(long, value_name = "URL")]
        pac: Option<String>,
        /// Hosts that bypass the proxy (comma or space separated)
        #[arg(long, value_name = "LIST")]
        no_proxy: Option<String>,
        /// Save HTML pages instead of refusing them as web pages
        #[arg(long)]
        allow_html: bool,
        /// Do not fall back to yt-dlp when the URL is a media page
        #[arg(long)]
        no_resolve: bool,
        /// Maximum video height for resolved media (0 = best available)
        #[arg(long, default_value_t = rg_resolve::DEFAULT_MAX_HEIGHT)]
        max_height: u32,
        /// Path to yt-dlp (defaults to PATH and common install locations)
        #[arg(long)]
        yt_dlp: Option<PathBuf>,
        /// Path to ffmpeg (needed when a video has a separate audio track)
        #[arg(long)]
        ffmpeg: Option<PathBuf>,
        /// Extra argument passed to yt-dlp, for example --yt-dlp-arg=--cookies-from-browser
        #[arg(long = "yt-dlp-arg", value_name = "ARG")]
        yt_dlp_args: Vec<String>,
        /// Scan the completed file with an external command (off by default)
        #[arg(long)]
        virus_check: bool,
        /// Virus-check command template with {file} placeholder (implies --virus-check)
        #[arg(long, value_name = "CMD")]
        virus_command: Option<String>,
    },
    /// Resolve a media page and print the selected streams without downloading
    Resolve {
        url: String,
        /// Maximum video height (0 = best available)
        #[arg(long, default_value_t = rg_resolve::DEFAULT_MAX_HEIGHT)]
        max_height: u32,
        /// Path to yt-dlp (defaults to PATH and common install locations)
        #[arg(long)]
        yt_dlp: Option<PathBuf>,
        /// Extra argument passed to yt-dlp
        #[arg(long = "yt-dlp-arg", value_name = "ARG")]
        yt_dlp_args: Vec<String>,
    },
    /// Show the persisted segment map for an output file
    Status { output: PathBuf },
    /// Native-messaging host: read one capture frame from stdin (length-
    /// prefixed JSON) and forward it to the GUI named pipe.
    Host,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("rg_core=info")),
        )
        .init();

    match Cli::parse().command {
        Command::Get {
            url,
            output,
            dir,
            connections,
            limit,
            restart,
            sha256,
            user_agent,
            referer,
            cookie,
            title,
            proxy,
            proxy_user,
            proxy_pass,
            user,
            password,
            system_proxy,
            pac,
            no_proxy,
            allow_html,
            no_resolve,
            max_height,
            yt_dlp,
            ffmpeg,
            yt_dlp_args,
            virus_check,
            virus_command,
        } => {
            let output = output.unwrap_or_else(|| {
                let base = dir
                    .or_else(dirs::download_dir)
                    .unwrap_or_else(|| PathBuf::from("."));
                let name = rg_core::default_capture_name(&rg_core::NamingContext {
                    title: title.as_deref(),
                    content_disposition: None,
                    filename: None,
                    url: &url,
                    referer: referer.as_deref(),
                    content_type: None,
                    media_capture: false,
                });
                let name = rg_core::unique_file_name(&base, &name);
                base.join(name)
            });
            let network = network_config(
                proxy,
                proxy_user,
                proxy_pass,
                user,
                password,
                system_proxy,
                pac,
                no_proxy,
            );
            let direct = direct_download(
                &url,
                &output,
                connections,
                limit,
                restart,
                sha256.as_deref(),
                user_agent.as_deref(),
                referer.as_deref(),
                cookie.as_deref(),
                allow_html,
                &network,
            )
            .await;

            let run_virus = virus_check || virus_command.as_ref().is_some_and(|s| !s.trim().is_empty());
            let virus_cmd = virus_command.clone();
            match direct {
                Ok(stats) => {
                    eprintln!();
                    println!(
                        "done: {} bytes in {:.2}s ({} segment(s), ranged={}) -> {}",
                        stats.total,
                        stats.elapsed.as_secs_f64(),
                        stats.segments,
                        stats.ranged,
                        stats.output.display()
                    );
                    if run_virus {
                        run_cli_virus_check(&stats.output, virus_cmd.as_deref());
                    }
                }
                Err(rg_core::Error::WebPage {
                    content_type,
                    url: page,
                }) if !allow_html && !no_resolve => {
                    eprintln!(
                        "server returned {content_type} for {page}; trying the media resolver (yt-dlp)"
                    );
                    if sha256.is_some() {
                        eprintln!("warning: --sha256 is ignored for streams resolved by yt-dlp");
                    }
                    let tools = Tools::resolve_paths(yt_dlp, ffmpeg)?;
                    let mut request = MediaRequest::new(url.clone(), output)
                        .connections(connections)
                        .restart(restart)
                        .network(network.clone())
                        .extra_args(yt_dlp_args);
                    if limit > 0 {
                        request = request.speed_limit(limit);
                    }
                    if max_height > 0 {
                        request = request.max_height(max_height);
                    }
                    let started = Instant::now();
                    let path =
                        rg_resolve::download_media(&tools, request, print_media_event).await?;
                    println!(
                        "done: {} bytes in {:.2}s -> {}",
                        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                        started.elapsed().as_secs_f64(),
                        path.display()
                    );
                    if run_virus {
                        run_cli_virus_check(&path, virus_cmd.as_deref());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Command::Resolve {
            url,
            max_height,
            yt_dlp,
            yt_dlp_args,
        } => {
            let tools = Tools::resolve_paths(yt_dlp, None)?;
            let selection = if max_height > 0 {
                Some(max_height)
            } else {
                None
            };
            let resolved = rg_resolve::resolve(
                &rg_resolve::SystemRunner,
                &tools.yt_dlp,
                &url,
                selection,
                &yt_dlp_args,
            )
            .await?;

            println!("title:    {}", resolved.title);
            if let Some(duration) = resolved.duration {
                println!("duration: {:.1}s", duration);
            }
            let selection = resolved.selection;
            println!(
                "video:    {} {} {} ({} bytes)",
                selection.video.format_id,
                selection.video.ext,
                selection
                    .video
                    .height
                    .map(|h| format!("{h}p"))
                    .unwrap_or_else(|| "unknown".to_string()),
                selection
                    .video
                    .filesize
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_string())
            );
            println!("  {}", selection.video.url);
            if let Some(audio) = &selection.audio {
                println!(
                    "audio:    {} {} ({} bytes)",
                    audio.format_id,
                    audio.ext,
                    audio
                        .filesize
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_string())
                );
                println!("  {}", audio.url);
            }
            println!(
                "output:   {} container{}",
                selection.ext,
                if selection.needs_merge {
                    ", merge with ffmpeg"
                } else {
                    ""
                }
            );
        }
        Command::Status { output } => {
            let path = rg_core::parts_dir_for(&output).join("manifest.json");
            match rg_core::Manifest::load(&path)? {
                Some(m) => {
                    println!("url:     {}", m.url);
                    println!("total:   {} bytes", m.total);
                    println!("etag:    {:?}", m.etag);
                    println!("segments:");
                    for s in &m.segments {
                        println!(
                            "  #{:<3} {:>12}..{:<12} pos={:<12} remaining={}",
                            s.id,
                            s.start,
                            s.end,
                            s.pos,
                            s.remaining()
                        );
                    }
                }
                None => println!("no partial download at {}", path.display()),
            }
        }
        Command::Host => {
            let request = rg_bridge::read_frame(std::io::stdin())?;
            let ack = match rg_bridge::send_capture(&request) {
                Ok(()) => rg_bridge::HostAck {
                    ok: true,
                    error: None,
                },
                Err(e) => rg_bridge::HostAck {
                    ok: false,
                    error: Some(format!("{e:#}")),
                },
            };
            rg_bridge::write_ack_frame(std::io::stdout(), &ack)?;
        }
    }

    Ok(())
}

/// Turn the proxy flags into the engine's network configuration.
fn network_config(
    proxy: Option<String>,
    proxy_user: Option<String>,
    proxy_pass: Option<String>,
    user: Option<String>,
    password: Option<String>,
    system_proxy: bool,
    pac: Option<String>,
    no_proxy: Option<String>,
) -> NetworkConfig {
    let mut config = ProxyConfig::default();
    if let Some(url) = proxy.filter(|value| !value.trim().is_empty()) {
        config.mode = ProxyMode::Manual;
        config.url = url;
    } else if system_proxy {
        config.mode = ProxyMode::System;
    }
    config.username = proxy_user.unwrap_or_default();
    config.password = proxy_pass.unwrap_or_default();
    config.pac_url = pac.unwrap_or_default();
    config.no_proxy = no_proxy.unwrap_or_default();
    let credentials = user
        .filter(|value| !value.trim().is_empty())
        .map(|username| rg_core::Credentials::new(username, password.unwrap_or_default()));
    NetworkConfig {
        proxy: config,
        credentials,
    }
}

#[allow(clippy::too_many_arguments)]
async fn direct_download(
    url: &str,
    output: &Path,
    connections: usize,
    limit: u64,
    restart: bool,
    sha256: Option<&str>,
    user_agent: Option<&str>,
    referer: Option<&str>,
    cookie: Option<&str>,
    allow_html: bool,
    network: &NetworkConfig,
) -> std::result::Result<rg_core::Stats, rg_core::Error> {
    let mut dl = Download::new(url.to_string(), output)
        .connections(connections)
        .resume(!restart)
        .allow_html(allow_html)
        .network(network.clone());
    if limit > 0 {
        dl = dl.speed_limit(limit);
    }
    if let Some(expected) = sha256 {
        dl = dl.sha256(expected.to_string());
    }
    if let Some(ua) = user_agent {
        dl = dl.user_agent(ua.to_string());
    }
    if let Some(r) = referer.filter(|s| !s.trim().is_empty()) {
        dl = dl.referer(r.to_string());
    }
    if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
        dl = dl.cookie(c.to_string());
    }
    dl = dl.on_progress(|p: Progress| {
        let pct = if p.total > 0 {
            p.downloaded as f64 * 100.0 / p.total as f64
        } else {
            0.0
        };
        eprint!(
            "\r{pct:>6.2}%  {:>9.2} MB / {:>9.2} MB  {:>7.2} MB/s  conns={}      ",
            p.downloaded as f64 / 1_048_576.0,
            p.total as f64 / 1_048_576.0,
            p.speed_bps / 1_048_576.0,
            p.connections
        );
    });
    dl.run().await
}

fn print_media_event(event: MediaEvent) {
    match event {
        MediaEvent::Resolving => eprintln!("resolving the page with yt-dlp..."),
        MediaEvent::Resolved {
            title,
            ext,
            height,
            needs_merge,
            total_bytes,
            ..
        } => {
            let size = total_bytes
                .map(|bytes| format!("{:.2} MB", bytes as f64 / 1_048_576.0))
                .unwrap_or_else(|| "unknown size".to_string());
            eprintln!(
                "resolved: {title} ({}{}, {size}, {})",
                height.map(|h| format!("{h}p ")).unwrap_or_default(),
                ext,
                if needs_merge {
                    "video + audio"
                } else {
                    "single stream"
                }
            );
        }
        MediaEvent::Progress(progress) => {
            let stage = match progress.stage {
                MediaStage::Video => "video",
                MediaStage::Audio => "audio",
                MediaStage::Merging => "merge",
            };
            eprint!(
                "\r{:>6.2}%  {:<6} {:>7.2} MB / {:>7.2} MB  {:>7.2} MB/s      ",
                progress.overall * 100.0,
                stage,
                progress.downloaded as f64 / 1_048_576.0,
                progress.total as f64 / 1_048_576.0,
                progress.speed_bps / 1_048_576.0
            );
        }
        MediaEvent::Merging => eprintln!("\nmerging video and audio with ffmpeg..."),
        MediaEvent::Done { output, bytes } => {
            eprintln!();
            eprintln!("saved {} bytes to {}", bytes, output.display());
        }
    }
}

fn split_cli_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has_content = false;
    let mut quote = '\0';
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == quote {
                quoted = false;
            } else if quote == '"' && ch == '\\' {
                match chars.next() {
                    Some('"') => current.push('"'),
                    Some(next) => { current.push('\\'); current.push(next); }
                    None => current.push('\\'),
                }
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch.is_whitespace() {
            if has_content { args.push(std::mem::take(&mut current)); has_content = false; }
        } else if ch == '\'' || ch == '"' {
            quote = ch; quoted = true; has_content = true;
        } else { current.push(ch); has_content = true; }
    }
    if has_content { args.push(current); }
    args
}

#[cfg(windows)]
fn find_defender_cli() -> Option<PathBuf> {
    let platform_root = PathBuf::from(r"C:\ProgramData\Microsoft\Windows Defender\Platform");
    if let Ok(entries) = std::fs::read_dir(&platform_root) {
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("MpCmdRun.exe"))
            .filter(|p| p.exists())
            .collect();
        candidates.sort();
        if let Some(p) = candidates.pop() { return Some(p); }
    }
    for fallback in [
        r"C:\Program Files\Windows Defender\MpCmdRun.exe",
        r"C:\Program Files (x86)\Windows Defender\MpCmdRun.exe",
    ] {
        let p = PathBuf::from(fallback);
        if p.exists() { return Some(p); }
    }
    None
}

#[cfg(not(windows))]
fn find_defender_cli() -> Option<PathBuf> { None }

fn run_cli_virus_check(file: &Path, command: Option<&str>) {
    let template = command
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            find_defender_cli().map(|p| format!("\"{}\" -Scan -ScanType 3 -File \"{{file}}\"", p.display()))
        });
    let Some(template) = template else {
        eprintln!("virus check: no scanner configured (pass --virus-command or install Windows Defender)");
        return;
    };
    let file_str = file.display().to_string();
    let substituted = if template.contains("{file}") {
        template.replace("{file}", &file_str)
    } else {
        format!("{} \"{}\"", template.trim(), file_str)
    };
    let parts = split_cli_args(&substituted);
    let Some((program, args)) = parts.split_first() else {
        eprintln!("virus check: empty command");
        return;
    };
    eprintln!("virus check: scanning {}", file.display());
    match std::process::Command::new(program).args(args).output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let text = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
            match out.status.code() {
                Some(0) => println!("virus check: clean"),
                Some(2) => {
                    println!("virus check: THREAT DETECTED");
                    if !text.is_empty() { println!("{text}"); }
                }
                code => {
                    println!("virus check: error (exit {:?})", code);
                    if !text.is_empty() { println!("{text}"); }
                }
            }
        }
        Err(e) => eprintln!("virus check: could not start scanner: {e}"),
    }
}
