pub mod animez_client;
pub mod cli;
pub mod config;
pub mod db;
pub mod dupe_checker;
pub mod flaresolverr;
pub mod models;
pub mod packager;
pub mod parser;
pub mod pipeline;
pub mod prowlarr;
pub mod qbittorrent;
pub mod torrent_builder;

use crate::cli::{Cli, Commands};
use crate::config::AppConfig;
use crate::models::ReleaseState;
use crate::parser::parse_anime_release;
use crate::pipeline::Pipeline;
use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::str::FromStr;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    if std::path::Path::new(".env").exists() {
        crate::config::load_env_file_safe(std::path::Path::new(".env"));
    } else {
        let _ = dotenvy::dotenv();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("zbrr=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config = AppConfig::load(cli.env_file.as_deref())?;
    let pipeline = Pipeline::new(config.clone())?;

    match cli.command {
        Commands::Run { interval_secs } => {
            pipeline.run_daemon(interval_secs, cli.dry_run).await?;
        }

        Commands::Once => {
            pipeline.run_cycle(cli.dry_run).await?;
        }

        Commands::InspectForm { output, from_html } => {
            let target_output = output
                .map(|p| crate::config::expand_tilde(&p))
                .unwrap_or_else(|| config.general.data_dir.join("upload_form_schema.json"));

            info!(
                output = %target_output.display(),
                "Inspecting animez.to upload HTML form structure"
            );
            let report = pipeline
                .animez()
                .inspect_upload_form(from_html.as_deref())
                .await?;

            if let Some(parent) = target_output.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            let json = serde_json::to_string_pretty(&report)?;
            fs::write(&target_output, &json).with_context(|| {
                format!(
                    "Failed to write form inspection JSON to {}",
                    target_output.display()
                )
            })?;

            println!(
                "Saved form inspection report to {} (forms={}, csrf_token={})",
                target_output.display(),
                report.forms.len(),
                report.csrf_token_present
            );
        }

        Commands::Doctor => {
            pipeline.test_all_connections().await?;
        }

        Commands::PackageLocal {
            path,
            output_dir,
            skip_screenshots,
        } => {
            let file_or_dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let parsed = parse_anime_release(file_or_dir_name);
            println!("=== Local Media Packaging Test ===");
            println!("Input Path      : {}", path.display());
            println!("Parsed Title    : {}", parsed.clean_title);
            println!("Release Group   : {:?}", parsed.release_group);
            println!(
                "Season/Episode  : S{:?}E{:?} (pack={})",
                parsed.season, parsed.episode, parsed.is_season_pack
            );
            println!("Resolution      : {:?}", parsed.resolution);
            println!("Normalized Key  : {}", parsed.normalized_key);

            let bundle = pipeline
                .packager()
                .package_release(&path, output_dir.as_deref(), skip_screenshots)
                .await?;

            println!("\n--- Package Bundle Created ---");
            println!("Torrent File    : {}", bundle.torrent_path.display());
            println!("InfoHash (SHA1) : {}", bundle.torrent_info_hash);
            println!("Payload Size    : {} bytes ({} files)", bundle.total_size_bytes, bundle.file_count);
            println!("Piece Length    : {} bytes", bundle.piece_length);
            println!("MediaInfo File  : {}", bundle.mediainfo_path.display());
            println!("Screenshots     : {}", bundle.screenshots.len());
            for shot in &bundle.screenshots {
                println!("  - {}", shot.display());
            }
        }

        Commands::Status { state, limit } => {
            let filter = match state {
                Some(ref s) => Some(ReleaseState::from_str(s)?),
                None => None,
            };

            let rows = pipeline.db().list_releases(filter, limit)?;
            if rows.is_empty() {
                println!("No tracked releases found in SQLite database.");
            } else {
                println!(
                    "{:<5} | {:<13} | {:<8} | {:<50} | UPDATED_AT",
                    "ID", "STATE", "MAL_ID", "TITLE"
                );
                println!("{}", "-".repeat(110));
                for r in rows {
                    let mal_str = r
                        .mal_id
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let short_title = if r.raw_title.chars().count() > 50 {
                        let truncated: String = r.raw_title.chars().take(47).collect();
                        format!("{truncated}...")
                    } else {
                        r.raw_title.clone()
                    };
                    println!(
                        "{:<5} | {:<13} | {:<8} | {:<50} | {}",
                        r.id,
                        r.state.as_str(),
                        mal_str,
                        short_title,
                        r.updated_at
                    );
                    if let Some(err) = &r.error_message {
                        println!("      └─ note/error: {err}");
                    }
                }
            }
        }

        Commands::Retry { id, now } => {
            let new_state = pipeline.db().reset_for_retry(id)?;
            println!(
                "Reset release #{id} to state {new_state}."
            );
            if now {
                pipeline.process_release_by_id(id, cli.dry_run).await?;
            }
        }
    }

    Ok(())
}
