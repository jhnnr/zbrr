use crate::config::AppConfig;
use crate::db::Database;
use crate::models::{MalMetadata, ParsedRelease};
use anyhow::{Context, Result};
use chrono::{Datelike, Utc};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, warn};

/// Normalizes a title into a lowercase underscore-separated alphanumeric string.
pub fn normalize_title(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
}

/// Determines if a string extracted from square brackets is likely a release/fansub group name
/// rather than a technical specification tag, container, audio/video codec, resolution, or CRC32 hash.
pub fn is_potential_release_group(candidate: &str) -> bool {
    let s = candidate.trim();
    if s.is_empty() || s.len() > 30 {
        return false;
    }

    // 1. CRC32 [8 hex characters]
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }

    // 2. 4-digit Year [19xx or 20xx]
    if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(y) = s.parse::<u32>() {
            if (1950..=2050).contains(&y) {
                return false;
            }
        }
    }

    let upper = s.to_ascii_uppercase().replace(['.', '-', '_'], "");

    // 3. Resolutions
    if matches!(
        upper.as_str(),
        "2160P" | "1080P" | "720P" | "576P" | "480P" | "4K" | "1920X1080" | "1280X720" | "3840X2160"
    ) {
        return false;
    }

    // 4. Video Codecs
    if matches!(
        upper.as_str(),
        "HEVC" | "X265" | "H265" | "AVC" | "X264" | "H264" | "AV1" | "XVID"
    ) {
        return false;
    }

    // 5. Sources / Rip types
    if matches!(
        upper.as_str(),
        "WEBDL" | "WEBRIP" | "BLURAY" | "BD" | "BDRIP" | "BRRIP" | "HDTV" | "DVD" | "DVDRIP" | "REMUX" | "CR" | "CRUNCHYROLL" | "NF" | "NETFLIX" | "AMZN" | "AMAZON" | "BGLOBAL" | "HIDI"
    ) {
        return false;
    }

    // 6. Audio Codecs & Channels
    if matches!(
        upper.as_str(),
        "AAC" | "FLAC" | "AC3" | "EAC3" | "DDP" | "OPUS" | "TRUEHD" | "DTS" | "DTSHD" | "DTSHDMA" | "MP3" | "DUALAUDIO" | "MULTIAUDIO" | "ENGLISHDUBBED" | "ENGDUB" | "DUBBED" | "DUB"
    ) || upper.starts_with("AAC") || upper.starts_with("FLAC") {
        return false;
    }

    // 7. Bit depth
    if matches!(upper.as_str(), "10BIT" | "8BIT" | "HI10P") {
        return false;
    }

    // 8. Subtitle / Language / Version / Pack tags
    if matches!(
        upper.as_str(),
        "MULTISUB" | "MULTISUBS" | "MSUB" | "MSUBS" | "SOFTSUBS" | "HARDSUBS" | "RAW" | "ENGSUB" | "SUB" | "SUBS" | "BATCH" | "COMPLETE" | "V1" | "V2" | "V3" | "V4"
    ) {
        return false;
    }

    // 9. HDR tags
    if matches!(upper.as_str(), "HDR" | "HDR10" | "HDR10+" | "DV" | "DOVI" | "DOLBYVISION" | "SDR") {
        return false;
    }

    // 10. If candidate contains spaces and words like "1080p", "CR", "WEBRip", it's a composite technical bracket
    if s.contains(' ') {
        let has_technical_tokens = s.split_whitespace().any(|tok| {
            let u = tok.to_ascii_uppercase().replace(['.', '-', '_'], "");
            matches!(
                u.as_str(),
                "1080P" | "720P" | "2160P" | "HEVC" | "X265" | "H265" | "AVC" | "X264" | "H264" | "WEBRIP" | "WEBDL" | "BLURAY" | "BD" | "AAC" | "FLAC" | "EAC3" | "CR" | "NF" | "AMZN"
            )
        });
        if has_technical_tokens {
            return false;
        }
    }

    true
}

/// Returns the canonical video codec string ("hevc", "h264", "av1", "xvid") for matching and key generation.
pub fn canonical_codec(vc: Option<&str>) -> &'static str {
    match vc {
        Some(s) => {
            let u = s.to_ascii_uppercase();
            if u.contains("HEVC") || u.contains("X265") || u.contains("H265") || u.contains("H.265") {
                "hevc"
            } else if u.contains("AV1") {
                "av1"
            } else if u.contains("XVID") {
                "xvid"
            } else {
                "h264"
            }
        }
        None => "h264",
    }
}

/// Parses an anime release filename or title into structured metadata.
pub fn parse_anime_release(raw_input: &str) -> ParsedRelease {
    let mut parsed = ParsedRelease {
        raw_title: raw_input.trim().to_string(),
        ..Default::default()
    };

    let mut working = raw_input
        .trim()
        .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .to_string();

    // 1. Strip known media container extension at the end
    static EXT_RE: OnceLock<Regex> = OnceLock::new();
    let ext_re = EXT_RE.get_or_init(|| Regex::new(r"(?i)\.(mkv|mp4|avi|m2ts|ts)$").unwrap());
    if let Some(caps) = ext_re.captures(&working) {
        parsed.container = Some(caps[1].to_ascii_uppercase());
        working = ext_re.replace(&working, "").to_string();
    }

    // 2. Extract CRC32 [8 hex chars]
    static CRC_RE: OnceLock<Regex> = OnceLock::new();
    let crc_re = CRC_RE.get_or_init(|| Regex::new(r"\[([0-9A-Fa-f]{8})\]").unwrap());
    if let Some(caps) = crc_re.captures(&working) {
        parsed.crc32 = Some(caps[1].to_ascii_uppercase());
        working = crc_re.replace(&working, " ").to_string();
    }

    // 3. Extract leading [ReleaseGroup]
    static LEAD_GROUP_RE: OnceLock<Regex> = OnceLock::new();
    let lead_group_re = LEAD_GROUP_RE.get_or_init(|| Regex::new(r"^\s*\[([^\[\]]+)\]\s*").unwrap());
    if let Some(caps) = lead_group_re.captures(&working) {
        let candidate = caps[1].trim().to_string();
        if is_potential_release_group(&candidate) {
            parsed.release_group = Some(candidate);
            working = lead_group_re.replace(&working, "").to_string();
        }
    }

    // 4. Resolution
    static RES_RE: OnceLock<Regex> = OnceLock::new();
    let res_re = RES_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(2160p|1080p|720p|576p|480p|4k|3840x2160|1920x1080|1280x720)\b").unwrap()
    });
    if let Some(caps) = res_re.captures(&working) {
        let r = match caps[1].to_ascii_lowercase().as_str() {
            "4k" | "3840x2160" | "2160p" => "2160p",
            "1920x1080" | "1080p" => "1080p",
            "1280x720" | "720p" => "720p",
            "576p" => "576p",
            "480p" => "480p",
            _ => "1080p",
        };
        parsed.resolution = Some(r.to_string());
    }

    // 5. Source (WEB-DL, WEBRip, BluRay, BDRip, HDTV, DVD)
    static SRC_RE: OnceLock<Regex> = OnceLock::new();
    let src_re = SRC_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(WEB-DL|WEBDL|WEBRip|WEB-Rip|Blu-?Ray|BDRip|BD|HDTV|DVDRip|CR|Crunchyroll|AMZN|NF|B-Global|HIDI)\b").unwrap()
    });
    if let Some(caps) = src_re.captures(&working) {
        let src_upper = caps[1].to_ascii_uppercase();
        let s = match src_upper.as_str() {
            "WEB-DL" | "WEBDL" | "CR" | "CRUNCHYROLL" | "AMZN" | "NF" | "B-GLOBAL" | "HIDI" => {
                "WEB-DL"
            }
            "WEBRIP" | "WEB-RIP" => "WEBRip",
            "BLURAY" | "BLU-RAY" | "BD" => "BluRay",
            "BDRIP" => "BDRip",
            "HDTV" => "HDTV",
            "DVDRIP" => "DVD",
            other => other,
        };
        parsed.source = Some(s.to_string());
    }

    // 6. Video Codec
    static VCODEC_RE: OnceLock<Regex> = OnceLock::new();
    let vcodec_re = VCODEC_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(HEVC|x265|H\.?265|AVC|x264|H\.?264|AV1|XviD)\b").unwrap()
    });
    if let Some(caps) = vcodec_re.captures(&working) {
        let vc = match caps[1].to_ascii_uppercase().replace('.', "").as_str() {
            "HEVC" | "X265" | "H265" => "HEVC",
            "AVC" | "X264" | "H264" => "H.264",
            "AV1" => "AV1",
            "XVID" => "XviD",
            _ => "H.264",
        };
        parsed.video_codec = Some(vc.to_string());
    }

    // 7. Bit Depth
    static BIT_RE: OnceLock<Regex> = OnceLock::new();
    let bit_re = BIT_RE.get_or_init(|| Regex::new(r"(?i)\b(10-?bit|8-?bit|Hi10p)\b").unwrap());
    if let Some(caps) = bit_re.captures(&working) {
        let b = if caps[1].to_ascii_lowercase().contains("10") {
            "10bit"
        } else {
            "8bit"
        };
        parsed.bit_depth = Some(b.to_string());
    }

    // 8. Audio Codec
    static ACODEC_RE: OnceLock<Regex> = OnceLock::new();
    let acodec_re = ACODEC_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(FLAC|E-?AC-?3|DDP|AAC|AC-?3|DD|Opus|TrueHD|DTS(?:-HD)?|MP3)(?:[\s._-]*[1257]\.[01])?\b",
        )
        .unwrap()
    });
    if let Some(caps) = acodec_re.captures(&working) {
        let ac = match caps[1]
            .to_ascii_uppercase()
            .replace('-', "")
            .as_str()
        {
            "FLAC" => "FLAC",
            "EAC3" | "DDP" => "EAC3",
            "AAC" => "AAC",
            "AC3" | "DD" => "AC3",
            "OPUS" => "Opus",
            "TRUEHD" => "TrueHD",
            s if s.starts_with("DTS") => "DTS",
            "MP3" => "MP3",
            _ => "AAC",
        };
        parsed.audio_codec = Some(ac.to_string());
    }

    // 9. Dual Audio / Multi-Sub flags
    static DUAL_RE: OnceLock<Regex> = OnceLock::new();
    let dual_re = DUAL_RE.get_or_init(|| Regex::new(r"(?i)\b(Dual[\s._-]?Audio)\b").unwrap());
    parsed.dual_audio = dual_re.is_match(&working);

    static MSUB_RE: OnceLock<Regex> = OnceLock::new();
    let msub_re = MSUB_RE
        .get_or_init(|| Regex::new(r"(?i)\b(Multi(?:ple)?[\s._-]?Sub(?:title)?s?|MSubs?)\b").unwrap());
    parsed.multi_sub = msub_re.is_match(&working);

    // 10. Year
    static YEAR_RE: OnceLock<Regex> = OnceLock::new();
    let year_re = YEAR_RE.get_or_init(|| Regex::new(r"[\(\[\s.](19\d{2}|20\d{2})[\)\]\s.]").unwrap());
    if let Some(caps) = year_re.captures(&working) {
        if let Ok(y) = caps[1].parse::<u32>() {
            parsed.year = Some(y);
        }
    }

    // 11. Extract release group if not already found from leading bracket:
    // Check all non-leading square brackets (standard AnimeZ / tracker naming convention: Title - 01 (Year) [Group] [Tags...])
    if parsed.release_group.is_none() {
        static ALL_BRACKETS_RE: OnceLock<Regex> = OnceLock::new();
        let all_brackets_re =
            ALL_BRACKETS_RE.get_or_init(|| Regex::new(r"\[([^\[\]]+)\]").unwrap());
        for caps in all_brackets_re.captures_iter(&working) {
            let candidate = caps[1].trim();
            if is_potential_release_group(candidate) {
                parsed.release_group = Some(candidate.to_string());
                break;
            }
        }
    }

    // Trailing scene-style group if not already found: e.g., "-VARYG" at end
    if parsed.release_group.is_none() {
        static TRAIL_GROUP_RE: OnceLock<Regex> = OnceLock::new();
        let trail_group_re =
            TRAIL_GROUP_RE.get_or_init(|| Regex::new(r"-([A-Za-z0-9_]{2,20})$").unwrap());
        if let Some(caps) = trail_group_re.captures(working.trim()) {
            let g = &caps[1];
            // Avoid matching resolution or codec tokens
            let upper = g.to_ascii_uppercase();
            if !matches!(
                upper.as_str(),
                "1080P" | "720P" | "2160P" | "X264" | "X265" | "HEVC" | "AVC" | "DL" | "RIP"
            ) {
                parsed.release_group = Some(g.to_string());
            }
        }
    }

    // 12. Remove all remaining [...] and (...) metadata blocks to isolate title + episode
    static BRACKET_RE: OnceLock<Regex> = OnceLock::new();
    let bracket_re =
        BRACKET_RE.get_or_init(|| Regex::new(r"(\[[^\[\]]*\]|\([^\(\)]*\))").unwrap());

    // Before stripping brackets, check if any bracket indicates Batch / Season
    static SEASON_PART_DASH_RE: OnceLock<Regex> = OnceLock::new();
    let season_part_dash_re = SEASON_PART_DASH_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:Season\s*\d{1,2}|Part\s*\d{1,2}|Cour\s*\d{1,2}|Vol(?:ume)?\s*\d{1,2}|S\d{1,2}|\d{1,2}(?:st|nd|rd|th)\s*Season|(?:First|Second|Third|Fourth|Fifth|Sixth)\s*Season)\s*-\s*").unwrap()
    });
    let working_no_season_prefix = season_part_dash_re.replace_all(&working, " ");

    static BATCH_RE: OnceLock<Regex> = OnceLock::new();
    let batch_re = BATCH_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(Batch|Complete|\d{1,3}\s*(?:-|~)\s*\d{1,3})\b").unwrap()
    });
    if batch_re.is_match(&working_no_season_prefix) {
        parsed.is_season_pack = true;
    }

    static SEASON_EXPLICIT_RE: OnceLock<Regex> = OnceLock::new();
    let season_explicit_re = SEASON_EXPLICIT_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:(\d{1,2})(?:st|nd|rd|th)\s*Season|(?:First|Second|Third|Fourth|Fifth|Sixth)\s*Season|Season\s*(\d{1,2}))\b").unwrap()
    });
    if let Some(caps) = season_explicit_re.captures(&working) {
        if let Some(num_str) = caps.get(1) {
            parsed.season = num_str.as_str().parse::<u32>().ok();
        } else if let Some(n) = caps.get(2) {
            parsed.season = n.as_str().parse::<u32>().ok();
        } else if let Some(word_match) = caps.get(0) {
            let lower = word_match.as_str().to_ascii_lowercase();
            if lower.contains("first") || lower.contains("1st") {
                parsed.season = Some(1);
            } else if lower.contains("second") || lower.contains("2nd") {
                parsed.season = Some(2);
            } else if lower.contains("third") || lower.contains("3rd") {
                parsed.season = Some(3);
            } else if lower.contains("fourth") || lower.contains("4th") {
                parsed.season = Some(4);
            } else if lower.contains("fifth") || lower.contains("5th") {
                parsed.season = Some(5);
            } else if lower.contains("sixth") || lower.contains("6th") {
                parsed.season = Some(6);
            }
        }
    }

    let stripped = bracket_re.replace_all(&working, " ").to_string();
    // Normalize dots/underscores to spaces when dots are used as word separators
    let normalized_spaces = if !stripped.contains(' ') && stripped.contains('.') {
        stripped.replace('.', " ")
    } else {
        stripped.replace('_', " ")
    };

    // 13. Extract Season & Episode patterns:
    // Pattern A: S01E02 or S01E02v2
    static SE_RE: OnceLock<Regex> = OnceLock::new();
    let se_re =
        SE_RE.get_or_init(|| Regex::new(r"(?i)\bS(\d{1,2})E(\d{1,4})(?:v\d+)?\b").unwrap());

    // Pattern B: S2 - 05 or Season 2 - 05 or 2nd Season - 05
    static S_DASH_E_RE: OnceLock<Regex> = OnceLock::new();
    let s_dash_e_re = S_DASH_E_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:S(\d{1,2})|Season\s*(\d{1,2})|(?P<ord>\d{1,2})(?:st|nd|rd|th)\s*Season|(?P<word>First|Second|Third|Fourth|Fifth|Sixth)\s*Season)\s*-\s*(\d{1,4})(?:v\d+)?\b",
        )
        .unwrap()
    });

    // Pattern C: " - 01 " or " - 01v2" at end of title
    static DASH_EP_RE: OnceLock<Regex> = OnceLock::new();
    let dash_ep_re =
        DASH_EP_RE.get_or_init(|| Regex::new(r"\s+-\s+(\d{1,4})(?:v\d+)?(?:\b|$)").unwrap());

    // Pattern D: Season-only pack "S01" / "Season 1"
    static S_ONLY_RE: OnceLock<Regex> = OnceLock::new();
    let s_only_re = S_ONLY_RE.get_or_init(|| Regex::new(r"(?i)\bS(\d{1,2})\b").unwrap());

    let mut title_cut_idx = normalized_spaces.len();

    if let Some(m) = se_re.find(&normalized_spaces) {
        let caps = se_re.captures(&normalized_spaces).unwrap();
        parsed.season = caps[1].parse::<u32>().ok();
        parsed.episode = caps[2].parse::<u32>().ok();
        title_cut_idx = title_cut_idx.min(m.start());
    } else if let Some(m) = s_dash_e_re.find(&normalized_spaces) {
        let caps = s_dash_e_re.captures(&normalized_spaces).unwrap();
        if let Some(ord) = caps.name("ord") {
            parsed.season = ord.as_str().parse::<u32>().ok();
        } else if let Some(word) = caps.name("word") {
            let lower = word.as_str().to_ascii_lowercase();
            parsed.season = match lower.as_str() {
                "first" => Some(1),
                "second" => Some(2),
                "third" => Some(3),
                "fourth" => Some(4),
                "fifth" => Some(5),
                "sixth" => Some(6),
                _ => None,
            };
        } else if let Some(s1) = caps.get(1) {
            parsed.season = s1.as_str().parse::<u32>().ok();
        } else if let Some(s2) = caps.get(2) {
            parsed.season = s2.as_str().parse::<u32>().ok();
        }
        let ep_idx = caps.len() - 1;
        parsed.episode = caps.get(ep_idx).and_then(|c| c.as_str().parse::<u32>().ok());
        title_cut_idx = title_cut_idx.min(m.start());
    } else if let Some(m) = dash_ep_re.find(&normalized_spaces) {
        let caps = dash_ep_re.captures(&normalized_spaces).unwrap();
        if parsed.season.is_none() {
            parsed.season = Some(1);
        }
        parsed.episode = caps[1].parse::<u32>().ok();
        title_cut_idx = title_cut_idx.min(m.start());
    } else if let Some(m) = s_only_re.find(&normalized_spaces) {
        let caps = s_only_re.captures(&normalized_spaces).unwrap();
        parsed.season = caps[1].parse::<u32>().ok();
        parsed.is_season_pack = true;
        title_cut_idx = title_cut_idx.min(m.start());
    }

    // Also cut before resolution or source tokens if they appeared in dot-separated scene release names
    if let Some(m) = res_re.find(&normalized_spaces) {
        title_cut_idx = title_cut_idx.min(m.start());
    }
    if let Some(m) = src_re.find(&normalized_spaces) {
        title_cut_idx = title_cut_idx.min(m.start());
    }

    let raw_clean = &normalized_spaces[..title_cut_idx];
    static MULTI_SPACE_RE: OnceLock<Regex> = OnceLock::new();
    let multi_space_re = MULTI_SPACE_RE.get_or_init(|| Regex::new(r"\s+").unwrap());
    let clean_title = multi_space_re
        .replace_all(
            raw_clean
                .trim()
                .trim_start_matches('#')
                .trim()
                .trim_end_matches(['-', '.', '_', '#']),
            " ",
        )
        .trim()
        .to_string();

    parsed.clean_title = if clean_title.is_empty() {
        raw_input.trim().trim_start_matches('#').trim().to_string()
    } else {
        clean_title
    };

    // Build canonical normalized key for deduplication
    let norm_title: String = parsed
        .clean_title
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");

    let ep_part = match (parsed.season, parsed.episode, parsed.is_season_pack) {
        (Some(s), Some(e), _) => format!("s{s:02}e{e:02}"),
        (Some(s), None, true) => format!("s{s:02}_pack"),
        (None, Some(e), _) => format!("s01e{e:02}"),
        _ if parsed.is_season_pack => "pack".to_string(),
        _ => "movie_or_ova".to_string(),
    };

    let codec_part = canonical_codec(parsed.video_codec.as_deref());
    let res_part = parsed
        .resolution
        .as_deref()
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    let group_part = parsed
        .release_group
        .as_deref()
        .unwrap_or("nogroup")
        .to_ascii_lowercase();

    parsed.normalized_key = format!("{norm_title}|{ep_part}|{res_part}|{codec_part}|{group_part}");
    parsed
}

/// Jikan v4 client for resolving MyAnimeList (MAL) IDs with local SQLite caching.
pub struct JikanResolver {
    http: Client,
    config: AppConfig,
    db: Database,
}

#[derive(Debug, Deserialize)]
struct JikanSearchResponse {
    data: Vec<JikanAnimeItem>,
}

#[derive(Debug, Deserialize)]
struct JikanSingleResponse {
    data: JikanAnimeItem,
}

#[derive(Debug, Deserialize)]
struct JikanAnimeItem {
    mal_id: u64,
    title: String,
    title_english: Option<String>,
    title_japanese: Option<String>,
    #[serde(rename = "type")]
    media_type: Option<String>,
    episodes: Option<u32>,
    year: Option<u32>,
    synopsis: Option<String>,
    images: Option<JikanImages>,
}

#[derive(Debug, Deserialize)]
struct JikanImages {
    jpg: Option<JikanImageSet>,
}

#[derive(Debug, Deserialize)]
struct JikanImageSet {
    large_image_url: Option<String>,
    image_url: Option<String>,
}

impl JikanResolver {
    pub fn new(config: AppConfig, db: Database) -> Result<Self> {
        let http = Client::builder()
            .user_agent("zbrr/0.1.0 (AnimeZ Automated Pipeline)")
            .timeout(Duration::from_secs(20))
            .build()
            .context("Failed to build Jikan HTTP client")?;
        Ok(Self { http, config, db })
    }

    /// Resolves canonical MAL metadata for a clean anime title, checking SQLite cache first.
    pub async fn resolve_mal(&self, clean_title: &str) -> Result<Option<MalMetadata>> {
        if !self.config.jikan.enabled || clean_title.trim().is_empty() {
            return Ok(None);
        }

        let search_key = clean_title.trim().to_ascii_lowercase();
        let ttl = self.config.jikan.cache_ttl_days;
        let db_clone = self.db.clone();
        let key_clone = search_key.clone();

        if let Some(cached) =
            tokio::task::spawn_blocking(move || db_clone.get_cached_mal(&key_clone, ttl)).await??
        {
            debug!(
                title = clean_title,
                mal_id = cached.mal_id,
                "Resolved MAL ID from SQLite cache"
            );
            return Ok(Some(cached));
        }

        // Enforce polite delay before hitting Jikan API
        self.config.polite_sleep().await;

        let base = self.config.jikan.base_url.trim_end_matches('/');
        let url = format!("{base}/anime");

        let resp = self
            .http
            .get(&url)
            .query(&[("q", clean_title.trim()), ("limit", "5"), ("sfw", "true")])
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, title = clean_title, "Jikan API request failed; continuing without MAL ID");
                return Ok(None);
            }
        };

        if !resp.status().is_success() {
            warn!(
                status = %resp.status(),
                title = clean_title,
                "Jikan API returned non-200 status"
            );
            return Ok(None);
        }

        let payload: JikanSearchResponse = match resp.json().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "Failed to parse Jikan JSON response");
                return Ok(None);
            }
        };

        let Some(first) = payload.data.into_iter().next() else {
            debug!(title = clean_title, "No MAL results found on Jikan");
            return Ok(None);
        };

        let image_url = first
            .images
            .and_then(|i| i.jpg)
            .and_then(|j| j.large_image_url.or(j.image_url));

        let meta = MalMetadata {
            mal_id: first.mal_id,
            title: first.title,
            title_english: first.title_english,
            title_japanese: first.title_japanese,
            media_type: first.media_type,
            episodes: first.episodes,
            year: first.year,
            synopsis: first.synopsis,
            image_url,
        };

        let db_save = self.db.clone();
        let meta_save = meta.clone();
        let _ =
            tokio::task::spawn_blocking(move || db_save.set_cached_mal(&search_key, &meta_save))
                .await;

        Ok(Some(meta))
    }

    /// Resolves canonical MAL metadata for an exact MyAnimeList ID, checking SQLite cache first.
    pub async fn resolve_mal_by_id(&self, mal_id: u64) -> Result<Option<MalMetadata>> {
        let cache_key = format!("mal_id_{mal_id}");
        let ttl = self.config.jikan.cache_ttl_days;
        let db_clone = self.db.clone();
        let key_clone = cache_key.clone();

        if let Some(cached) =
            tokio::task::spawn_blocking(move || db_clone.get_cached_mal(&key_clone, ttl)).await??
        {
            debug!(
                mal_id = mal_id,
                "Resolved MAL metadata by ID from SQLite cache"
            );
            return Ok(Some(cached));
        }

        if !self.config.jikan.enabled {
            let meta = MalMetadata {
                mal_id,
                title: String::new(),
                title_english: None,
                title_japanese: None,
                media_type: None,
                episodes: None,
                year: None,
                synopsis: None,
                image_url: None,
            };
            return Ok(Some(meta));
        }

        // Enforce polite delay before hitting Jikan API
        self.config.polite_sleep().await;

        let base = self.config.jikan.base_url.trim_end_matches('/');
        let url = format!("{base}/anime/{mal_id}");

        let resp = self.http.get(&url).send().await;
        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!(status = %r.status(), mal_id = mal_id, "Jikan API returned non-success status for anime ID");
                let meta = MalMetadata {
                    mal_id,
                    title: String::new(),
                    title_english: None,
                    title_japanese: None,
                    media_type: None,
                    episodes: None,
                    year: None,
                    synopsis: None,
                    image_url: None,
                };
                return Ok(Some(meta));
            }
            Err(e) => {
                warn!(error = %e, mal_id = mal_id, "Jikan API request failed for anime ID");
                let meta = MalMetadata {
                    mal_id,
                    title: String::new(),
                    title_english: None,
                    title_japanese: None,
                    media_type: None,
                    episodes: None,
                    year: None,
                    synopsis: None,
                    image_url: None,
                };
                return Ok(Some(meta));
            }
        };

        let payload: JikanSingleResponse = match resp.json().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, mal_id = mal_id, "Failed to parse Jikan single anime JSON");
                return Ok(None);
            }
        };

        let item = payload.data;
        let image_url = item
            .images
            .and_then(|i| i.jpg)
            .and_then(|j| j.large_image_url.or(j.image_url));

        let meta = MalMetadata {
            mal_id: item.mal_id,
            title: item.title,
            title_english: item.title_english,
            title_japanese: item.title_japanese,
            media_type: item.media_type,
            episodes: item.episodes,
            year: item.year,
            synopsis: item.synopsis,
            image_url,
        };

        let db_save = self.db.clone();
        let meta_save = meta.clone();
        let key_save = cache_key.clone();
        let _ =
            tokio::task::spawn_blocking(move || db_save.set_cached_mal(&key_save, &meta_save))
                .await;

        Ok(Some(meta))
    }
}

/// Extracted links pointing to external anime databases (MAL, AniDB) and fansub information pages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalAnimeLinks {
    pub mal_id: Option<u64>,
    pub mal_url: Option<String>,
    pub anidb_id: Option<u64>,
    pub anidb_url: Option<String>,
    pub information_url: Option<String>,
}

/// Parses HTML (e.g. from a Nyaa view page or fansub information page) to extract
/// canonical MyAnimeList, AniDB, and Information links.
pub fn parse_external_anime_links(html: &str) -> ExternalAnimeLinks {
    let mut links = ExternalAnimeLinks::default();

    // 1. Direct MAL URL: https://myanimelist.net/anime/<id>
    static MAL_URL_RE: OnceLock<Regex> = OnceLock::new();
    let mal_re = MAL_URL_RE.get_or_init(|| {
        Regex::new(r#"(?i)https?://(?:www\.)?myanimelist\.net/anime/(\d+)"#).unwrap()
    });
    if let Some(caps) = mal_re.captures(html) {
        if let Ok(id) = caps[1].parse::<u64>() {
            links.mal_id = Some(id);
            links.mal_url = Some(format!("https://myanimelist.net/anime/{id}"));
        }
    }

    // 2. Direct AniDB URL: https://anidb.net/anime/<id> or https://anidb.net/a<id>
    static ANIDB_URL_RE: OnceLock<Regex> = OnceLock::new();
    let anidb_re = ANIDB_URL_RE.get_or_init(|| {
        Regex::new(r#"(?i)https?://(?:www\.)?anidb\.net/(?:anime/|perl-bin/animedb\.pl\?show=anime&aid=|a)(\d+)"#).unwrap()
    });
    if let Some(caps) = anidb_re.captures(html) {
        if let Ok(id) = caps[1].parse::<u64>() {
            links.anidb_id = Some(id);
            links.anidb_url = Some(format!("https://anidb.net/anime/{id}"));
        }
    }

    // 3. Information Link on Nyaa:
    // e.g. <div class="col-md-1">Information:</div>\s*<div class="col-md-5">\s*<a href="https://www.erai-raws.info/anime-list/...">
    static NYAA_INFO_RE: OnceLock<Regex> = OnceLock::new();
    let info_re = NYAA_INFO_RE.get_or_init(|| {
        Regex::new(r#"(?i)Information:[\s\S]*?<a[^>]+href=["']?(https?://[^"'>\s]+)"#).unwrap()
    });
    if let Some(caps) = info_re.captures(html) {
        let raw_url = caps[1].trim();
        // Ignore self-referential nyaa links or generic search links
        if !raw_url.contains("nyaa.si") {
            links.information_url = Some(raw_url.to_string());
        }
    }

    // Also check for fansub info links (like Erai-raws anime-list) in description / body
    if links.information_url.is_none() {
        static FANSUB_INFO_RE: OnceLock<Regex> = OnceLock::new();
        let fansub_re = FANSUB_INFO_RE.get_or_init(|| {
            Regex::new(r#"(?i)https?://(?:www\.)?erai-raws\.info/anime-list/[^"'\s\)]+"#).unwrap()
        });
        if let Some(caps) = fansub_re.captures(html) {
            links.information_url = Some(caps[0].trim().to_string());
        }
    }

    links
}

/// Asynchronously resolves external anime links (MAL ID/URL and AniDB ID/URL) starting from a release URL
/// (e.g. Nyaa view page or fansub information page).
pub async fn resolve_external_links_from_url(
    http: &Client,
    url: &str,
) -> Result<ExternalAnimeLinks> {
    let clean_url = url.trim();
    if clean_url.is_empty() || !clean_url.starts_with("http") {
        return Ok(ExternalAnimeLinks::default());
    }

    let resp = http
        .get(clean_url)
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .timeout(Duration::from_secs(12))
        .send()
        .await;

    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            debug!(status = %r.status(), url = clean_url, "External URL returned non-success HTTP status");
            return Ok(ExternalAnimeLinks::default());
        }
        Err(e) => {
            debug!(error = %e, url = clean_url, "Failed fetching external URL");
            return Ok(ExternalAnimeLinks::default());
        }
    };

    let body = resp.text().await.unwrap_or_default();
    let mut links = parse_external_anime_links(&body);

    // If MAL ID wasn't found directly on the page, but an Information URL (e.g. Erai-raws anime-list) was found:
    if links.mal_id.is_none() {
        if let Some(info_url) = links.information_url.clone() {
            debug!(info_url = %info_url, "Following Information link to extract MAL and AniDB links");
            let sub_resp = http
                .get(&info_url)
                .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .timeout(Duration::from_secs(12))
                .send()
                .await;

            if let Ok(sub_r) = sub_resp {
                if sub_r.status().is_success() {
                    let sub_body = sub_r.text().await.unwrap_or_default();
                    let sub_links = parse_external_anime_links(&sub_body);
                    if sub_links.mal_id.is_some() {
                        links.mal_id = sub_links.mal_id;
                        links.mal_url = sub_links.mal_url;
                    }
                    if sub_links.anidb_id.is_some() {
                        links.anidb_id = sub_links.anidb_id;
                        links.anidb_url = sub_links.anidb_url;
                    }
                }
            }
        }
    }

    Ok(links)
}

/// Queries Nyaa.si search to locate a release's canonical view page URL (`https://nyaa.si/view/<id>`)
/// by raw title or clean anime title.
pub async fn find_nyaa_view_url(http: &Client, query: &str) -> Option<String> {
    let clean = query.trim();
    if clean.is_empty() {
        return None;
    }

    let mut url = match url::Url::parse("https://nyaa.si/") {
        Ok(u) => u,
        Err(_) => return None,
    };
    url.query_pairs_mut()
        .append_pair("f", "0")
        .append_pair("c", "0_0")
        .append_pair("q", clean);

    let resp = http
        .get(url.as_str())
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let text = resp.text().await.ok()?;
    static VIEW_RE: OnceLock<Regex> = OnceLock::new();
    let re = VIEW_RE.get_or_init(|| Regex::new(r#"/view/(\d+)"#).unwrap());
    if let Some(caps) = re.captures(&text) {
        let id = &caps[1];
        return Some(format!("https://nyaa.si/view/{id}"));
    }
    None
}

/// Formats a torrent release title strictly according to AnimeZ official naming standards:
/// https://animez.to/guides/how-to-properly-titlename-a-torrent
///
/// Rules:
/// - Anime - Ongoing:
///   `[AniDB Main Title] - [Episode] (Year) [Fansub] [Additional Tags...]`
///   If the anime includes the year as part of the AniDB main title:
///   `[AniDB Main Title with (Year)] - [Episode] [Fansub] [Additional Tags...]`
/// - Anime - Season Batch / Partial Batch:
///   `[AniDB Main Title] (Season xx) - [Episodes] (Year) [Fansub] [Additional Tags...]`
/// - Anime - Complete:
///   `[AniDB Main Title] (Year) [Fansub] [Additional Tags...]`
///
/// Extracts the start and end episode from an episode range in a batch title (e.g. "02-07" -> (2, 7) or "01~12" -> (1, 12)).
pub fn parse_batch_episode_range(title: &str) -> Option<(u32, u32)> {
    // 1. Remove Season patterns like "Season 2 - " or "S02 - " or "2nd Season - " first
    static SEASON_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
    let season_prefix_re = SEASON_PREFIX_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:Season\s*\d{1,2}|Part\s*\d{1,2}|Cour\s*\d{1,2}|Vol(?:ume)?\s*\d{1,2}|S\d{1,2}|\d{1,2}(?:st|nd|rd|th)\s*Season|(?:First|Second|Third|Fourth|Fifth|Sixth)\s*Season)\s*-\s*").unwrap()
    });
    let cleaned = season_prefix_re.replace_all(title, " ");

    // 2. Search for episode range like "02-07" or "01~12"
    static BATCH_RANGE_RE: OnceLock<Regex> = OnceLock::new();
    let re = BATCH_RANGE_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(\d{1,4})\s*(?:-|~)\s*(\d{1,4})\b").unwrap()
    });
    for caps in re.captures_iter(&cleaned) {
        if let (Ok(start), Ok(end)) = (caps[1].parse::<u32>(), caps[2].parse::<u32>()) {
            if start < end {
                return Some((start, end));
            }
        }
    }
    None
}

/// Permitted Additional Tags (strictly enforced, in canonical order):
///   `[Dual Audio]`, `[Multi Audio]`, `[English Dubbed]`, `[RAW]`, `[HDR10]` / `[HDR10+]`, `[HEVC]`, `[10-bit]`
pub fn format_animez_torrent_title(
    parsed: &ParsedRelease,
    mal: Option<&MalMetadata>,
    animez_title: Option<&str>,
) -> String {
    // 1. Determine canonical base title (prefer AnimeZ tracker entry / MAL Romaji / AniDB title)
    let raw_title = if let Some(at) = animez_title {
        if !at.trim().is_empty() {
            at.trim().to_string()
        } else if let Some(m) = mal {
            if !m.title.trim().is_empty() {
                m.title.trim().to_string()
            } else {
                parsed.clean_title.trim().to_string()
            }
        } else {
            parsed.clean_title.trim().to_string()
        }
    } else if let Some(m) = mal {
        if !m.title.trim().is_empty() {
            m.title.trim().to_string()
        } else {
            parsed.clean_title.trim().to_string()
        }
    } else {
        parsed.clean_title.trim().to_string()
    };

    static YEAR_IN_PAREN_RE: OnceLock<Regex> = OnceLock::new();
    let year_in_paren_re = YEAR_IN_PAREN_RE.get_or_init(|| {
        Regex::new(r"\((?:19\d{2}|20\d{2})\)").unwrap()
    });
    let title_has_year = year_in_paren_re.is_match(&raw_title);

    let release_year = parsed
        .year
        .or_else(|| mal.and_then(|m| m.year))
        .unwrap_or_else(|| Utc::now().year() as u32);

    let mut title_part = raw_title.clone();

    // Check if season should be added if not already part of the title
    static SEASON_RE: OnceLock<Regex> = OnceLock::new();
    let season_re = SEASON_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:Season\s*\d+|2nd\s*Season|3rd\s*Season|\d+th\s*Season)\b").unwrap()
    });

    if let Some(s) = parsed.season {
        if s > 1 && !season_re.is_match(&title_part) && !title_has_year {
            title_part = format!("{title_part} (Season {s:02})");
        }
    }

    // 2. Format episode / batch portion
    let mut ep_part = String::new();
    if parsed.is_season_pack {
        // Try extracting episode range like "01-12" or "02-07" from raw title
        if let Some((start, end)) = parse_batch_episode_range(&parsed.raw_title) {
            if start < 100 && end < 100 {
                ep_part = format!("- {:02}-{:02}", start, end);
            } else {
                ep_part = format!("- {start}-{end}");
            }
        }
    } else if let Some(ep) = parsed.episode {
        if ep < 100 {
            ep_part = format!("- {:02}", ep);
        } else {
            ep_part = format!("- {}", ep);
        }
    }

    // 3. Assemble core title with Year
    let core = if !ep_part.is_empty() {
        if title_has_year {
            format!("{title_part} {ep_part}")
        } else {
            format!("{title_part} {ep_part} ({release_year})")
        }
    } else if title_has_year {
        title_part
    } else {
        format!("{title_part} ({release_year})")
    };

    // 4. Fansub Group
    let group = parsed
        .release_group
        .as_deref()
        .unwrap_or("AnimeZ")
        .trim();
    let group_tag = format!("[{group}]");

    // 5. Allowed Additional Tags in canonical order:
    // [Dual Audio] / [Multi Audio] / [English Dubbed] / [RAW]
    // [HDR10] / [HDR10+]
    // [HEVC]
    // [10-bit]
    let mut additional_tags = Vec::new();
    let raw_upper = parsed.raw_title.to_ascii_uppercase();

    // Audio tags
    if parsed.dual_audio || raw_upper.contains("DUAL AUDIO") || raw_upper.contains("DUAL-AUDIO") {
        additional_tags.push("[Dual Audio]");
    } else if raw_upper.contains("MULTI AUDIO") || raw_upper.contains("MULTI-AUDIO") {
        additional_tags.push("[Multi Audio]");
    } else if raw_upper.contains("ENGLISH DUBBED") || raw_upper.contains("ENG DUB") {
        additional_tags.push("[English Dubbed]");
    } else if raw_upper.contains("[RAW]") || raw_upper.contains("-RAW ") || raw_upper.contains(" RAW ") {
        additional_tags.push("[RAW]");
    }

    // Codec tag (HEVC)
    let is_hevc = parsed
        .video_codec
        .as_deref()
        .map(|vc| {
            let u = vc.to_ascii_uppercase();
            u.contains("HEVC") || u.contains("X265") || u.contains("H265") || u.contains("H.265")
        })
        .unwrap_or(false)
        || raw_upper.contains("HEVC")
        || raw_upper.contains("X265")
        || raw_upper.contains("H265");
    if is_hevc {
        additional_tags.push("[HEVC]");
    }

    // HDR tag
    if raw_upper.contains("HDR10+") {
        additional_tags.push("[HDR10+]");
    } else if raw_upper.contains("HDR10") || raw_upper.contains("[HDR]") {
        additional_tags.push("[HDR10]");
    }

    // Bit depth tag
    let is_10bit = parsed
        .bit_depth
        .as_deref()
        .map(|b| b.contains("10"))
        .unwrap_or(false)
        || raw_upper.contains("10BIT")
        || raw_upper.contains("10-BIT");
    if is_10bit {
        additional_tags.push("[10-bit]");
    }

    let mut parts = vec![core, group_tag];
    for tag in additional_tags {
        parts.push(tag.to_string());
    }

    parts.join(" ")
}

/// Validates an anime release against AnimeZ official Upload Rules (UPLOAD_RULES.md).
/// Rejects containers, video codecs, resolutions, or release types prohibited by the tracker.
pub fn check_animez_rule_compliance(parsed: &ParsedRelease) -> std::result::Result<(), String> {
    // 1. Prohibited leaks / pre-airs (Section 1)
    static LEAK_RE: OnceLock<Regex> = OnceLock::new();
    let leak_re = LEAK_RE.get_or_init(|| Regex::new(r"(?i)\b(pre-?air|leak)\b").unwrap());
    if leak_re.is_match(&parsed.raw_title) {
        return Err("Pre-air leaks are strictly prohibited by AnimeZ upload rules (Section 1)".to_string());
    }

    // 2. Container verification (Section 2)
    // Allowed: MKV, MP4, AVI, M2TS, VOB, TS
    if let Some(c) = &parsed.container {
        let upper = c.to_ascii_uppercase();
        if !matches!(upper.as_str(), "MKV" | "MP4" | "AVI" | "M2TS" | "VOB" | "TS") {
            return Err(format!(
                "Container '{c}' is not permitted by AnimeZ upload rules; only MKV, MP4, AVI (and full disc M2TS/VOB) are allowed"
            ));
        }
    }

    // 3. Video Codec verification (Section 2)
    // Allowed: H264/AVC, H265/HEVC (MPEG2 for DVD/HDTV, VC-1 for BD)
    // Prohibited: AV1, VP9, VP8, XVID, DIVX
    if let Some(vc) = &parsed.video_codec {
        let upper = vc.to_ascii_uppercase();
        if upper.contains("AV1") || upper.contains("VP9") || upper.contains("VP8") || upper.contains("XVID") || upper.contains("DIVX") {
            return Err(format!(
                "Video codec '{vc}' is not permitted by AnimeZ upload rules; only H264/AVC and H265/HEVC are allowed for anime encodes"
            ));
        }
    }

    // 4. Minimum Resolution verification (Section 2: minimum 480p)
    if let Some(res) = &parsed.resolution {
        let lower = res.to_ascii_lowercase();
        if lower.contains("360p") || lower.contains("240p") || lower.contains("144p") {
            return Err(format!(
                "Resolution '{res}' is below AnimeZ minimum required resolution (480p)"
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_subsplease_release() {
        let p = parse_anime_release(
            "[SubsPlease] Sousou no Frieren - 28 (1080p) [F4A8B912].mkv",
        );
        assert_eq!(p.release_group.as_deref(), Some("SubsPlease"));
        assert_eq!(p.clean_title, "Sousou no Frieren");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(28));
        assert_eq!(p.resolution.as_deref(), Some("1080p"));
        assert_eq!(p.crc32.as_deref(), Some("F4A8B912"));
        assert_eq!(p.container.as_deref(), Some("MKV"));
        assert_eq!(
            p.normalized_key,
            "sousou_no_frieren|s01e28|1080p|h264|subsplease"
        );
    }

    #[test]
    fn test_parse_animez_standard_format_titles() {
        // AnimeZ standard naming: Title - 01 (Year) [Group]
        let p1 = parse_anime_release("Romelia Senki - 01 (2026) [SubsPlease]");
        assert_eq!(p1.clean_title, "Romelia Senki");
        assert_eq!(p1.episode, Some(1));
        assert_eq!(p1.release_group.as_deref(), Some("SubsPlease"));
        assert_eq!(p1.video_codec.as_deref(), None);

        // Leading '#' from tracker badge/table formatting
        let p2 = parse_anime_release("#Zombie Sagashitemasu - 01 (2026) [SubsPlease]");
        assert_eq!(p2.clean_title, "Zombie Sagashitemasu");
        assert_eq!(p2.episode, Some(1));
        assert_eq!(p2.release_group.as_deref(), Some("SubsPlease"));

        // AnimeZ title with [Group] and [HEVC] tag
        let p3 = parse_anime_release("Marronnier Oukoku no Shichinin no Kishi - 07 (2026) [Erai-raws] [HEVC]");
        assert_eq!(p3.clean_title, "Marronnier Oukoku no Shichinin no Kishi");
        assert_eq!(p3.episode, Some(7));
        assert_eq!(p3.release_group.as_deref(), Some("Erai-raws"));
        assert_eq!(p3.video_codec.as_deref(), Some("HEVC"));

        // AnimeZ title with numeric group name [030]
        let p4 = parse_anime_release("Sister Breeder - 01-06 (2026) [030] [HEVC] [10-bit]");
        assert_eq!(p4.clean_title, "Sister Breeder");
        assert!(p4.is_season_pack);
        assert_eq!(p4.release_group.as_deref(), Some("030"));
        assert_eq!(p4.video_codec.as_deref(), Some("HEVC"));
        assert_eq!(p4.bit_depth.as_deref(), Some("10bit"));
    }

    #[test]
    fn test_parse_scene_dot_release() {
        let p = parse_anime_release(
            "Solo.Leveling.S02E04.1080p.CR.WEB-DL.AAC2.0.H.264-VARYG.mkv",
        );
        assert_eq!(p.clean_title, "Solo Leveling");
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(4));
        assert_eq!(p.resolution.as_deref(), Some("1080p"));
        assert_eq!(p.source.as_deref(), Some("WEB-DL"));
        assert_eq!(p.video_codec.as_deref(), Some("H.264"));
        assert_eq!(p.audio_codec.as_deref(), Some("AAC"));
        assert_eq!(p.release_group.as_deref(), Some("VARYG"));
    }

    #[test]
    fn test_parse_batch_hevc_release() {
        let p = parse_anime_release(
            "[Judas] Cyberpunk Edgerunners (Season 1) [1080p][HEVC x265 10bit][Dual-Audio][Multi-Subs] (Batch)",
        );
        assert_eq!(p.release_group.as_deref(), Some("Judas"));
        assert_eq!(p.clean_title, "Cyberpunk Edgerunners");
        assert_eq!(p.season, Some(1));
        assert!(p.is_season_pack);
        assert_eq!(p.video_codec.as_deref(), Some("HEVC"));
        assert_eq!(p.bit_depth.as_deref(), Some("10bit"));
        assert!(p.dual_audio);
        assert!(p.multi_sub);
    }

    #[test]
    fn test_format_animez_torrent_title_ongoing() {
        let p = parse_anime_release(
            "[Erai-raws] Marronnier Oukoku no Shichinin no Kishi - 07 [1080p AMZN WEBRip HEVC EAC3][MultiSub][671E6865].mkv",
        );
        let title = format_animez_torrent_title(&p, None, None);
        // Clean ongoing format: Title - Ep (Year) [Group] [HEVC]
        assert_eq!(
            title,
            format!("Marronnier Oukoku no Shichinin no Kishi - 07 ({}) [Erai-raws] [HEVC]", Utc::now().year())
        );
    }

    #[test]
    fn test_format_animez_torrent_title_with_year_in_title() {
        let p = parse_anime_release(
            "[Erai-raws] Ranma 1/2 (2024) - 07 [1080p NF WEBRip HEVC AAC][MultiSub][44A0F064].mkv",
        );
        let title = format_animez_torrent_title(&p, None, Some("Ranma 1/2 (2024)"));
        // Does not duplicate year if title already contains (2024)
        assert_eq!(title, "Ranma 1/2 (2024) - 07 [Erai-raws] [HEVC]");
    }

    #[test]
    fn test_format_animez_torrent_title_batch() {
        let p = parse_anime_release(
            "[030] Sister Breeder - 01-06 [1080p HEVC 10-bit] (Batch)",
        );
        let title = format_animez_torrent_title(&p, None, None);
        assert_eq!(
            title,
            format!("Sister Breeder - 01-06 ({}) [030] [HEVC] [10-bit]", Utc::now().year())
        );
    }

    #[test]
    fn test_parse_yasei_no_last_boss_season_2() {
        let p = parse_anime_release(
            "[Erai-raws] Yasei no Last Boss ga Arawareta 2nd Season - 01 [1080p CR WEBRip HEVC AAC][MultiSub][2D919573].mkv",
        );
        assert_eq!(p.release_group.as_deref(), Some("Erai-raws"));
        assert_eq!(p.clean_title, "Yasei no Last Boss ga Arawareta");
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.video_codec.as_deref(), Some("HEVC"));

        // When AnimeZ canonical AniDB title is used
        let title_with_animez = format_animez_torrent_title(
            &p,
            None,
            Some("Yasei no Last Boss ga Arawareta! (2026)"),
        );
        assert_eq!(
            title_with_animez,
            "Yasei no Last Boss ga Arawareta! (2026) - 01 [Erai-raws] [HEVC]"
        );
    }

    #[test]
    fn test_check_animez_rule_compliance() {
        let valid = parse_anime_release("[SubsPlease] Title - 01 (1080p) [HEVC].mkv");
        assert!(check_animez_rule_compliance(&valid).is_ok());

        let leak = parse_anime_release("[Leak] Title - 01 Pre-air [1080p].mkv");
        assert!(check_animez_rule_compliance(&leak).is_err());

        let mut bad_codec = parse_anime_release("[Group] Title - 01 (1080p).mkv");
        bad_codec.video_codec = Some("AV1".to_string());
        assert!(check_animez_rule_compliance(&bad_codec).is_err());

        let mut low_res = parse_anime_release("[Group] Title - 01 (360p).mkv");
        low_res.resolution = Some("360p".to_string());
        assert!(check_animez_rule_compliance(&low_res).is_err());
    }

    #[test]
    fn test_batch_range_and_no_double_year() {
        // Range parsing
        assert_eq!(parse_batch_episode_range("[Erai-raws] Ao Ashi Season 2 - 02-07 [1080p]"), Some((2, 7)));
        assert_eq!(parse_batch_episode_range("[Erai-raws] Ao Ashi Season 2 - 01-07 [1080p]"), Some((1, 7)));
        assert_eq!(parse_batch_episode_range("Frieren 01~12 (Batch)"), Some((1, 12)));
        assert_eq!(parse_batch_episode_range("[SubsPlease] Frieren - 05 (1080p).mkv"), None);

        // Ensure Dia no Ace does not duplicate year (2026)
        let p_dia = parse_anime_release(
            "[Erai-raws] Diamond no Ace: Act II Second Season Part 2 - 06 [1080p CR WEB-DL AVC AAC][MultiSub][5C2E7EEF].mkv"
        );
        let title_dia = format_animez_torrent_title(
            &p_dia,
            None,
            Some("Dia no Ace: Act II Second Season (2026)"),
        );
        assert_eq!(
            title_dia,
            "Dia no Ace: Act II Second Season (2026) - 06 [Erai-raws]"
        );
        assert_eq!(title_dia.matches("(2026)").count(), 1, "Year (2026) must not be duplicated");
    }

    #[test]
    fn test_parse_external_anime_links() {
        let nyaa_html = r#"
            <div class="row">
                <div class="col-md-1">Information:</div>
                <div class="col-md-5">
                    <a rel="noopener noreferrer nofollow" href="https://www.erai-raws.info/anime-list/kashita-maryoku-wa-revo-barai-de-kyousei-choushuu/">https://www.erai-raws.info/anime-list/kashita-maryoku-wa-revo-barai-de-kyousei-choushuu/</a>
                </div>
            </div>
        "#;
        let links = parse_external_anime_links(nyaa_html);
        assert_eq!(
            links.information_url.as_deref(),
            Some("https://www.erai-raws.info/anime-list/kashita-maryoku-wa-revo-barai-de-kyousei-choushuu/")
        );
        assert_eq!(links.mal_id, None);

        let erai_html = r#"
            <div class=entry-sub-content-buttons>
                <a class=entry-sub-content-buttons target=_blank href=https://myanimelist.net/anime/62922>MAL</a>
                <a class=entry-sub-content-buttons target=_blank href=https://anidb.net/anime/19714>AniDB</a>
            </div>
        "#;
        let erai_links = parse_external_anime_links(erai_html);
        assert_eq!(erai_links.mal_id, Some(62922));
        assert_eq!(erai_links.mal_url.as_deref(), Some("https://myanimelist.net/anime/62922"));
        assert_eq!(erai_links.anidb_id, Some(19714));
        assert_eq!(erai_links.anidb_url.as_deref(), Some("https://anidb.net/anime/19714"));

        let direct_html = r#"
            <p>Source link: <a href="https://myanimelist.net/anime/52991">Frieren on MAL</a></p>
        "#;
        let direct_links = parse_external_anime_links(direct_html);
        assert_eq!(direct_links.mal_id, Some(52991));
        assert_eq!(direct_links.mal_url.as_deref(), Some("https://myanimelist.net/anime/52991"));
    }
}
