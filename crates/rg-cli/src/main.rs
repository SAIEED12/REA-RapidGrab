use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rg_core::{Download, Progress};

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
    },
    /// Show the persisted segment map for an output file
    Status { output: PathBuf },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        } => {
            let mut dl = Download::new(url, output.clone())
                .connections(connections)
                .resume(!restart);
            if limit > 0 {
                dl = dl.speed_limit(limit);
            }
            if let Some(expected) = sha256 {
                dl = dl.sha256(expected);
            }
            if let Some(ua) = user_agent {
                dl = dl.user_agent(ua);
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

            let stats = dl.run().await?;
            eprintln!();
            println!(
                "done: {} bytes in {:.2}s ({} segment(s), ranged={})",
                stats.total,
                stats.elapsed.as_secs_f64(),
                stats.segments,
                stats.ranged
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
