use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// Pipeline state machine for every discovered release:
/// DISCOVERED -> DUPE_CHECKED -> DOWNLOADING -> PACKAGING -> UPLOADING -> SEEDING
///                    |               |             |            |
///                    v               v             v            v
///                 SKIPPED          FAILED        FAILED       FAILED
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReleaseState {
    Discovered,
    DupeChecked,
    Downloading,
    Packaging,
    Uploading,
    Seeding,
    Skipped,
    Failed,
}

impl ReleaseState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReleaseState::Discovered => "DISCOVERED",
            ReleaseState::DupeChecked => "DUPE_CHECKED",
            ReleaseState::Downloading => "DOWNLOADING",
            ReleaseState::Packaging => "PACKAGING",
            ReleaseState::Uploading => "UPLOADING",
            ReleaseState::Seeding => "SEEDING",
            ReleaseState::Skipped => "SKIPPED",
            ReleaseState::Failed => "FAILED",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ReleaseState::Seeding | ReleaseState::Skipped | ReleaseState::Failed
        )
    }
}

impl fmt::Display for ReleaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReleaseState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "DISCOVERED" => Ok(ReleaseState::Discovered),
            "DUPE_CHECKED" => Ok(ReleaseState::DupeChecked),
            "DOWNLOADING" => Ok(ReleaseState::Downloading),
            "PACKAGING" => Ok(ReleaseState::Packaging),
            "UPLOADING" => Ok(ReleaseState::Uploading),
            "SEEDING" => Ok(ReleaseState::Seeding),
            "SKIPPED" => Ok(ReleaseState::Skipped),
            "FAILED" => Ok(ReleaseState::Failed),
            other => anyhow::bail!("Unknown ReleaseState: {other}"),
        }
    }
}

/// Raw release item discovered from Prowlarr API or Torznab RSS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredRelease {
    pub guid: String,
    pub title: String,
    pub indexer_id: i64,
    pub indexer_name: String,
    pub download_url: Option<String>,
    pub magnet_url: Option<String>,
    pub info_hash: Option<String>,
    pub size_bytes: u64,
    pub seeders: u32,
    pub leechers: u32,
    pub publish_date: Option<DateTime<Utc>>,
    pub info_url: Option<String>,
}

/// Structured metadata extracted from an anime release filename.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParsedRelease {
    pub raw_title: String,
    pub clean_title: String,
    pub normalized_key: String,
    pub release_group: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub is_season_pack: bool,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub video_codec: Option<String>,
    pub bit_depth: Option<String>,
    pub audio_codec: Option<String>,
    pub dual_audio: bool,
    pub multi_sub: bool,
    pub crc32: Option<String>,
    pub year: Option<u32>,
    pub container: Option<String>,
}

/// Canonical anime metadata resolved via Jikan API v4 (MyAnimeList).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MalMetadata {
    pub mal_id: u64,
    pub title: String,
    pub title_english: Option<String>,
    pub title_japanese: Option<String>,
    pub media_type: Option<String>,
    pub episodes: Option<u32>,
    pub year: Option<u32>,
    pub synopsis: Option<String>,
    pub image_url: Option<String>,
}

/// Full release record persisted in SQLite (`data/state.db`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub id: i64,
    pub guid: String,
    pub raw_title: String,
    pub normalized_key: String,
    pub parsed_json: String,
    pub indexer_id: i64,
    pub indexer_name: String,
    pub download_url: Option<String>,
    pub magnet_url: Option<String>,
    pub info_url: Option<String>,
    pub source_info_hash: Option<String>,
    pub tracker_info_hash: Option<String>,
    pub mal_id: Option<u64>,
    pub size_bytes: u64,
    pub seeders: u32,
    pub state: ReleaseState,
    pub save_path: Option<String>,
    pub content_path: Option<String>,
    pub packaged_torrent_path: Option<String>,
    pub mediainfo_path: Option<String>,
    pub screenshots_json: Option<String>,
    pub tracker_torrent_url: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: u32,
    pub created_at: String,
    pub updated_at: String,
}

impl ReleaseRecord {
    pub fn parsed(&self) -> anyhow::Result<ParsedRelease> {
        Ok(serde_json::from_str(&self.parsed_json)?)
    }
}

/// Result of checking whether a release already exists on animez.to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DupeCheckResult {
    pub is_dupe: bool,
    pub reason: String,
    pub matched_torrent_title: Option<String>,
    pub matched_torrent_url: Option<String>,
}

/// Local staging package produced by `packager.rs` + `torrent_builder.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageBundle {
    pub torrent_path: PathBuf,
    pub torrent_info_hash: String,
    pub mediainfo_path: PathBuf,
    pub mediainfo_text: String,
    pub screenshots: Vec<PathBuf>,
    pub content_path: PathBuf,
    pub primary_video_path: PathBuf,
    pub total_size_bytes: u64,
    pub file_count: usize,
    pub piece_length: u64,
}

/// Result of uploading a release to animez.to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadResult {
    pub success: bool,
    pub dry_run: bool,
    pub torrent_id: Option<String>,
    pub torrent_page_url: Option<String>,
    pub download_torrent_url: Option<String>,
    pub downloaded_tracker_torrent_path: Option<PathBuf>,
    pub uploaded_image_urls: Vec<String>,
}

/// Schema dumped by `zbrr inspect-form` for verifying `animez.to/upload` HTML structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormInspectionReport {
    pub url: String,
    pub page_title: String,
    pub inspected_at: DateTime<Utc>,
    pub csrf_token_present: bool,
    pub forms: Vec<FormElementSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormElementSchema {
    pub id: Option<String>,
    pub name: Option<String>,
    pub action: Option<String>,
    pub method: Option<String>,
    pub enctype: Option<String>,
    pub fields: Vec<FormFieldSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormFieldSchema {
    pub tag: String,
    pub field_type: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
    pub required: bool,
    pub placeholder: Option<String>,
    pub value: Option<String>,
    pub options: Vec<SelectOptionSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOptionSchema {
    pub value: String,
    pub label: String,
    pub selected: bool,
}
