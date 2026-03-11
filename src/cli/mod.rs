use crate::{
    engine::{config::EngineConfig, manager::DownloadManager, task::TaskOptions},
    rpc::server::{self, AppState},
};
use anyhow::Result;
use clap::{Parser, Subcommand};
use url::Url;

#[derive(Parser)]
#[command(
    name = "rude",
    about = "High-performance async download engine",
    version,
    author
)]
pub struct Cli {
    /// Path to config file (default: ~/.config/rude/config.toml)
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the background download daemon + RPC server
    Daemon,

    /// Add a URL (or mirror list) to the queue
    Add {
        /// One or more URLs (first is primary, rest are mirrors)
        #[arg(required = true)]
        urls: Vec<String>,
        /// Output filename
        #[arg(short, long)]
        out: Option<String>,
        /// Output directory
        #[arg(short, long)]
        dir: Option<String>,
        /// Number of connections per server
        #[arg(short = 'x', long, default_value = "8")]
        connections: usize,
        /// Number of chunks to split into
        #[arg(short = 's', long)]
        split: Option<usize>,
        /// Sequential (streaming) download — chunks in order
        #[arg(long)]
        sequential: bool,
        /// Max download speed (bytes/sec, 0 = unlimited)
        #[arg(long, default_value = "0")]
        max_speed: u64,
        /// Expected checksum (e.g. sha256=abc123...)
        #[arg(long)]
        checksum: Option<String>,
    },

    /// Show status of a download (by GID)
    Status {
        gid: String,
    },

    /// List all downloads
    List,

    /// Pause a download
    Pause {
        gid: String,
    },

    /// Resume a paused download
    Resume {
        gid: String,
    },

    /// Cancel and optionally remove a download
    Cancel {
        gid: String,
        /// Also delete the partial file
        #[arg(long)]
        remove_file: bool,
    },

    /// Print current config
    Config,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = EngineConfig::load()?;

    match cli.command {
        Commands::Daemon => run_daemon(config).await,
        Commands::Add {
            urls,
            out,
            dir,
            connections,
            split,
            sequential,
            max_speed,
            checksum,
        } => {
            let parsed_urls: Vec<Url> = urls
                .iter()
                .map(|u| Url::parse(u).map_err(anyhow::Error::from))
                .collect::<Result<_>>()?;

            let checksum_parsed = checksum
                .as_deref()
                .map(parse_checksum)
                .transpose()?;

            let options = TaskOptions {
                out,
                dir: dir.as_deref().map(std::path::PathBuf::from),
                split,
                max_connections: Some(connections),
                sequential,
                max_speed,
                checksum: checksum_parsed,
                ..Default::default()
            };

            // For CLI mode we run a single-shot manager without a server
            let manager = DownloadManager::new(config.clone());
            let id = manager.add(parsed_urls, options)?;
            println!("Queued: {}", id);

            // Wait for completion with progress display
            wait_for_completion(&manager, &id).await;
            Ok(())
        }
        Commands::List => {
            // When running as a client, we'd talk to the daemon over HTTP.
            // Shown here as the local variant.
            println!("Use 'rude daemon' to start the background server.");
            println!("API: GET http://127.0.0.1:16800/api/tellActive");
            Ok(())
        }
        Commands::Status { gid } => {
            println!("API: GET http://127.0.0.1:{}/api/tellStatus/{}", config.rpc.port, gid);
            Ok(())
        }
        Commands::Pause { gid } => {
            println!("API: PUT http://127.0.0.1:{}/api/pause/{}", config.rpc.port, gid);
            Ok(())
        }
        Commands::Resume { gid } => {
            println!("API: PUT http://127.0.0.1:{}/api/resume/{}", config.rpc.port, gid);
            Ok(())
        }
        Commands::Cancel { gid, remove_file } => {
            println!(
                "API: DELETE http://127.0.0.1:{}/api/remove/{} (remove_file={})",
                config.rpc.port, gid, remove_file
            );
            Ok(())
        }
        Commands::Config => {
            println!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
    }
}

async fn run_daemon(config: EngineConfig) -> Result<()> {
    tracing::info!("Starting rude daemon v{}", env!("CARGO_PKG_VERSION"));

    let manager = DownloadManager::new(config.clone());
    manager.restore_session().await?;
    manager.start_session_saver();

    let state = AppState { manager, config };
    server::serve(state).await?;
    Ok(())
}

async fn wait_for_completion(manager: &DownloadManager, id: &crate::engine::task::TaskId) {
    use std::time::Duration;
    use tokio::time::sleep;

    loop {
        if let Some(task) = manager.get(id) {
            match &task.state {
                crate::engine::task::DownloadState::Complete => {
                    println!(
                        "\n✓ Complete: {}",
                        task.output_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    );
                    break;
                }
                crate::engine::task::DownloadState::Error(e) => {
                    eprintln!("\n✗ Error: {}", e);
                    break;
                }
                state => {
                    // Show live progress
                    if let Some(rx) = manager.subscribe(id) {
                        if let Some(p) = rx.borrow().as_ref() {
                            let pct = p.total_bytes.map(|t| {
                                format!("{:.1}%", p.downloaded_bytes as f64 / t as f64 * 100.0)
                            }).unwrap_or_else(|| format!("{} B", p.downloaded_bytes));

                            let speed = format_speed(p.speed_bps);
                            let eta = p.eta_secs
                                .map(format_eta)
                                .unwrap_or_else(|| "?".into());

                            print!("\r  {:?}  {}  {}  ETA {}  [{} conn]   ",
                                state, pct, speed, eta, p.connections);
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    }
                }
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
}

fn format_speed(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} MB/s", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.1} KB/s", bps as f64 / 1_000.0)
    } else {
        format!("{} B/s", bps)
    }
}

fn format_eta(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

fn parse_checksum(
    s: &str,
) -> Result<crate::engine::task::Checksum> {
    use crate::engine::task::{Checksum, ChecksumAlgo};
    let (algo, value) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Checksum format: <algo>=<hex>"))?;
    let algorithm = match algo.to_lowercase().as_str() {
        "sha256" => ChecksumAlgo::Sha256,
        "sha1"   => ChecksumAlgo::Sha1,
        "md5"    => ChecksumAlgo::Md5,
        other    => return Err(anyhow::anyhow!("Unknown checksum algo: {}", other)),
    };
    Ok(Checksum { algorithm, value: value.to_string() })
}
