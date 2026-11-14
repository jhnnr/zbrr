use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "zbrr",
    version,
    about = "Automated Prowlarr-to-AnimeZ.to RSS/API release pipeline",
    long_about = "Single-binary Rust automation service and CLI tool that monitors Prowlarr for anime releases, performs duplicate checks against animez.to, downloads via qBittorrent, packages completed media (parallel Bencode .torrent, MediaInfo, FFmpeg screenshots, MAL metadata), uploads to animez.to via FlareSolverr + session cookies, and auto-reseeds."
)]
pub struct Cli {
    /// Optional path to environment file (.env) [default: .env or ~/.zbrr/.env]
    #[arg(short = 'e', long = "env", visible_alias = "config", global = true)]
    pub env_file: Option<PathBuf>,

    /// Global dry-run flag (executes discovery, dupe-check, download, and packaging without live upload POST)
    #[arg(long, global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start the continuous background polling daemon
    Run {
        /// Override poll interval in seconds
        #[arg(long)]
        interval_secs: Option<u64>,
    },

    /// Run a single poll, download, package, and upload cycle
    Once,

    /// Fetch animez.to/upload via FlareSolverr + cookies and dump HTML form schema to JSON
    InspectForm {
        /// Path to write the JSON form inspection report (defaults to ~/.zbrr/upload_form_schema.json)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Optional local HTML file to inspect offline instead of live fetching
        #[arg(long)]
        from_html: Option<PathBuf>,
    },

    /// Verify external binaries (ffmpeg, ffprobe, mediainfo) and service connectivity
    #[command(visible_alias = "test-connections")]
    Doctor,

    /// Test creating a .torrent + MediaInfo + PNG screenshots on a local file or folder
    PackageLocal {
        /// Path to local video file or release directory
        path: PathBuf,

        /// Optional custom output directory (defaults to data/staging/<release_name>)
        #[arg(short, long)]
        output_dir: Option<PathBuf>,

        /// Skip screenshot extraction (for quick .torrent + MediaInfo testing)
        #[arg(long)]
        skip_screenshots: bool,
    },

    /// List tracked releases in the SQLite state database
    #[command(visible_alias = "list")]
    Status {
        /// Filter by release state (e.g. DISCOVERED, DOWNLOADING, SEEDING, SKIPPED, FAILED)
        #[arg(short, long)]
        state: Option<String>,

        /// Maximum number of rows to display
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },

    /// Retry a FAILED or SKIPPED release by its database ID
    Retry {
        /// Database ID of the release to retry
        id: i64,

        /// Run the pipeline immediately after resetting the release state
        #[arg(long)]
        now: bool,
    },
}
