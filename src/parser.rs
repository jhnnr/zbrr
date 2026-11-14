use crate::config::AppConfig;
use crate::db::Database;
use crate::models::{MalMetadata, ParsedRelease};
use anyhow::{Context, Result};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, warn};

/// Parses an anime release filename or title into structured metadata.
pub fn parse_anime_release(raw_input: &str) -> ParsedRelease {
    let mut parsed = ParsedRelease {
        raw_title: raw_input.trim().to_string(),
        ..Default::default()
    };

    let mut working = raw_input.trim().to_string();

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
        parsed.release_group = Some(candidate);
        working = lead_group_re.replace(&working, "").to_string();
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

    // 11. Trailing scene-style group if not already found: e.g., "-VARYG" at end
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
    static BATCH_RE: OnceLock<Regex> = OnceLock::new();
    let batch_re = BATCH_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(Batch|Complete|\d{1,3}\s*-\s*\d{1,3})\b").unwrap()
    });
    if batch_re.is_match(&working) {
        parsed.is_season_pack = true;
    }

    static SEASON_PAREN_RE: OnceLock<Regex> = OnceLock::new();
    let season_paren_re =
        SEASON_PAREN_RE.get_or_init(|| Regex::new(r"(?i)\bSeason\s*(\d{1,2})\b").unwrap());
    if let Some(caps) = season_paren_re.captures(&working) {
        if let Ok(s) = caps[1].parse::<u32>() {
            parsed.season = Some(s);
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

    // Pattern B: S2 - 05 or Season 2 - 05
    static S_DASH_E_RE: OnceLock<Regex> = OnceLock::new();
    let s_dash_e_re = S_DASH_E_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:S|Season\s*)(\d{1,2})\s*-\s*(\d{1,4})(?:v\d+)?\b").unwrap()
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
        parsed.season = caps[1].parse::<u32>().ok();
        parsed.episode = caps[2].parse::<u32>().ok();
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
        .replace_all(raw_clean.trim().trim_end_matches(['-', '.', '_']), " ")
        .trim()
        .to_string();

    parsed.clean_title = if clean_title.is_empty() {
        raw_input.trim().to_string()
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

    parsed.normalized_key = format!("{norm_title}|{ep_part}|{res_part}|{group_part}");
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
            "sousou_no_frieren|s01e28|1080p|subsplease"
        );
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
}
