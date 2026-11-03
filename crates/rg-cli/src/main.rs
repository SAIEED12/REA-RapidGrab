use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rg_core::{Download, Progress};
use rg_resolve::{MediaEvent, MediaRequest, MediaStage, Tools};

#[derive(Parser)]
#[command(name = "rapidgrab", version, about = "Multi-segment resumable HTTP downloader")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL (resumes automatically when parts exist)
    Get {
        url: String,
        /// Output file path
        #[arg(short, long)]
        output: PathBuf,
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
            connections,
            limit,
            restart,
            sha256,
            user_agent,
            allow_html,
            no_resolve,
            max_height,
            yt_dlp,
            ffmpeg,
            yt_dlp_args,
        } => {
            let direct = direct_download(
                &url,
                &output,
                connections,
                limit,
                restart,
                sha256.as_deref(),
                user_agent.as_deref(),
                allow_html,
            )
            .await;

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
    }

    Ok(())
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
    allow_html: bool,
) -> std::result::Result<rg_core::Stats, rg_core::Error> {
    let mut dl = Download::new(url.to_string(), output)
        .connections(connections)
        .resume(!restart)
        .allow_html(allow_html);
    if limit > 0 {
        dl = dl.speed_limit(limit);
    }
    if let Some(expected) = sha256 {
        dl = dl.sha256(expected.to_string());
    }
    if let Some(ua) = user_agent {
        dl = dl.user_agent(ua.to_string());
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
