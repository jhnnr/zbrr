use crate::animez_client::{AnimezClient, TrackerTorrentEntry};
use crate::config::AppConfig;
use crate::db::Database;
use crate::models::{DupeCheckResult, ParsedRelease};
use crate::parser::parse_anime_release;
use crate::prowlarr::ProwlarrClient;
use anyhow::Result;
use tracing::{debug, info, warn};

pub struct DupeChecker {
    config: AppConfig,
    prowlarr: ProwlarrClient,
    animez: AnimezClient,
}

impl DupeChecker {
    pub fn new(config: AppConfig, prowlarr: ProwlarrClient, animez: AnimezClient) -> Self {
        Self {
            config,
            prowlarr,
            animez,
        }
    }

    /// Checks `animez.to` (via Prowlarr indexer if configured, plus direct HTTP search)
    /// to verify whether the same episode/pack + resolution + group already exists.
    pub async fn check_remote_duplicate(
        &self,
        parsed: &ParsedRelease,
        mal_id: Option<u64>,
    ) -> Result<DupeCheckResult> {
        // 1. Optional Prowlarr animez.to indexer lookup (auto-detects AnimeZ indexer if 0)
        let animez_idx = if self.config.prowlarr.animez_indexer_id > 0 {
            self.config.prowlarr.animez_indexer_id
        } else {
            self.prowlarr.get_animez_indexer_id().await.unwrap_or(0)
        };
        if animez_idx > 0 {
            debug!(
                indexer_id = animez_idx,
                title = %parsed.clean_title,
                "Checking animez.to duplicates via Prowlarr indexer"
            );
            match self
                .prowlarr
                .search_indexer(animez_idx, &parsed.clean_title)
                .await
            {
                Ok(items) => {
                    let entries: Vec<TrackerTorrentEntry> = items
                        .into_iter()
                        .map(|i| {
                            let p = parse_anime_release(&i.title);
                            TrackerTorrentEntry {
                                title: i.title,
                                url: i.guid,
                                resolution: p.resolution,
                                group: p.release_group,
                                video_codec: p.video_codec,
                            }
                        })
                        .collect();

                    if let Some(matched) = Self::find_matching_entry(parsed, &entries) {
                        info!(
                            candidate = %parsed.raw_title,
                            existing = %matched.title,
                            "Duplicate found via Prowlarr animez.to indexer"
                        );
                        return Ok(DupeCheckResult {
                            is_dupe: true,
                            reason: format!(
                                "Existing release on animez.to (Prowlarr): {}",
                                matched.title
                            ),
                            matched_torrent_title: Some(matched.title),
                            matched_torrent_url: Some(matched.url),
                        });
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "Prowlarr animez.to indexer dupe search failed; falling back to direct HTTP search"
                    );
                }
            }
        }

        // 2. Direct HTTP search on animez.to/torrents
        let entries = self
            .animez
            .search_torrents(&parsed.clean_title, mal_id)
            .await?;

        if let Some(matched) = Self::find_matching_entry(parsed, &entries) {
            info!(
                candidate = %parsed.raw_title,
                existing = %matched.title,
                "Duplicate found via direct animez.to search"
            );
            return Ok(DupeCheckResult {
                is_dupe: true,
                reason: format!("Existing release on animez.to: {}", matched.title),
                matched_torrent_title: Some(matched.title),
                matched_torrent_url: Some(matched.url),
            });
        }

        Ok(DupeCheckResult {
            is_dupe: false,
            reason: "No matching release found on animez.to".to_string(),
            matched_torrent_title: None,
            matched_torrent_url: None,
        })
    }

    /// Enforces AnimeZ Section 3 Ongoing Catch-Up Rule:
    /// If an ongoing anime has reached episode > 1, a user cannot start uploading standalone mid-season episodes
    /// without either having uploaded the prior episodes (01..N-1) or uploading a catch-up batch (01..N).
    pub async fn check_ongoing_catchup_rule(
        &self,
        parsed: &ParsedRelease,
        mal_id: Option<u64>,
        db: &Database,
    ) -> Result<Option<String>> {
        if !self.config.animez.enforce_ongoing_catchup {
            return Ok(None);
        }

        // Batches and movie/specials are permitted without ongoing sequence checks
        if parsed.is_season_pack {
            return Ok(None);
        }

        let Some(ep) = parsed.episode else {
            return Ok(None);
        };

        // Episode 1 (or 0) starts a sequence, always permitted
        if ep <= 1 {
            return Ok(None);
        }

        // 1. Check local SQLite DB for prior SEEDING uploads for this anime
        let db_clone = db.clone();
        let clean_title = parsed.clean_title.clone();
        let season = parsed.season;
        let has_local_prior = tokio::task::spawn_blocking(move || {
            db_clone.has_prior_upload_for_anime(&clean_title, mal_id, season)
        })
        .await??;

        if has_local_prior {
            debug!(
                title = %parsed.clean_title,
                episode = ep,
                "Prior local seeding upload found for anime; ongoing sequence satisfied"
            );
            return Ok(None);
        }

        // 2. Check AnimeZ tracker profile uploads
        let username = self.config.animez.username.trim();
        if !username.is_empty() && !username.starts_with("REPLACE_") && !username.starts_with("your_") {
            match self
                .animez
                .has_user_uploaded_prior_episode(username, &parsed.clean_title)
                .await
            {
                Ok(true) => {
                    info!(
                        title = %parsed.clean_title,
                        episode = ep,
                        uploader = username,
                        "Prior upload found on AnimeZ tracker profile; ongoing sequence satisfied"
                    );
                    return Ok(None);
                }
                Ok(false) => {}
                Err(e) => {
                    debug!(
                        error = %e,
                        uploader = username,
                        "Could not check profile uploads on AnimeZ; relying on local database"
                    );
                }
            }
        }

        let reason = format!(
            "AnimeZ upload rules (Section 3) catch-up violation: episode {ep} > 1 cannot be uploaded mid-season without prior sequence (episodes 01~{ep}); upload a catch-up batch of all available episodes first"
        );
        Ok(Some(reason))
    }

    pub fn find_matching_entry(
        candidate: &ParsedRelease,
        existing_entries: &[TrackerTorrentEntry],
    ) -> Option<TrackerTorrentEntry> {
        for entry in existing_entries {
            let mut existing_parsed = parse_anime_release(&entry.title);
            if existing_parsed.resolution.is_none() && entry.resolution.is_some() {
                existing_parsed.resolution = entry.resolution.clone();
            }
            if existing_parsed.release_group.is_none() && entry.group.is_some() {
                existing_parsed.release_group = entry.group.clone();
            }
            if existing_parsed.video_codec.is_none() && entry.video_codec.is_some() {
                existing_parsed.video_codec = entry.video_codec.clone();
            }
            if Self::is_same_release(candidate, &existing_parsed) {
                return Some(entry.clone());
            }
        }
        None
    }

    /// Compares two parsed releases to determine if they represent the same release
    /// (same anime title + season/episode/pack + resolution + video codec + release group).
    pub fn is_same_release(a: &ParsedRelease, b: &ParsedRelease) -> bool {
        if a.normalized_key == b.normalized_key {
            return true;
        }

        let title_a = normalize_alnum(&a.clean_title);
        let title_b = normalize_alnum(&b.clean_title);
        if title_a.is_empty() || title_a != title_b {
            return false;
        }

        // Compare season & episode / pack
        let season_a = a.season.unwrap_or(1);
        let season_b = b.season.unwrap_or(1);
        if season_a != season_b {
            return false;
        }
        if a.is_season_pack != b.is_season_pack {
            return false;
        }
        if !a.is_season_pack && a.episode != b.episode {
            return false;
        }

        // Compare resolution
        if let (Some(ra), Some(rb)) = (&a.resolution, &b.resolution) {
            if !ra.eq_ignore_ascii_case(rb) {
                return false;
            }
        }

        // Compare video codec (e.g. HEVC vs H.264/AVC can coexist per AnimeZ upload rules)
        let codec_a = crate::parser::canonical_codec(a.video_codec.as_deref());
        let codec_b = crate::parser::canonical_codec(b.video_codec.as_deref());
        if codec_a != codec_b {
            return false;
        }

        // Compare release group if both have one (different groups are NOT duplicates)
        if let (Some(ga), Some(gb)) = (&a.release_group, &b.release_group) {
            if !ga.eq_ignore_ascii_case(gb) {
                return false;
            }
        }

        true
    }
}

pub fn normalize_alnum(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dupe_matching_rules() {
        let candidate = parse_anime_release(
            "[SubsPlease] Sousou no Frieren - 12 (1080p) [ABCDEF12].mkv",
        );

        let same_diff_crc = parse_anime_release(
            "[SubsPlease] Sousou no Frieren - S01E12 (1080p) [99999999].mkv",
        );
        assert!(DupeChecker::is_same_release(&candidate, &same_diff_crc));

        let diff_ep = parse_anime_release(
            "[SubsPlease] Sousou no Frieren - 13 (1080p) [ABCDEF12].mkv",
        );
        assert!(!DupeChecker::is_same_release(&candidate, &diff_ep));

        let diff_res = parse_anime_release(
            "[SubsPlease] Sousou no Frieren - 12 (720p) [ABCDEF12].mkv",
        );
        assert!(!DupeChecker::is_same_release(&candidate, &diff_res));

        let diff_group = parse_anime_release(
            "[Erai-raws] Sousou no Frieren - 12 (1080p) [ABCDEF12].mkv",
        );
        assert!(!DupeChecker::is_same_release(&candidate, &diff_group));

        // AnimeZ tracker standard title tests (Romelia Senki / SubsPlease vs Erai-raws)
        let erai_romelia = parse_anime_release(
            "[Erai-raws] Romelia Senki - 01 [1080p CR WEBRip HEVC AAC][MultiSub][AAD97B02].mkv",
        );
        let subsplease_romelia = parse_anime_release("Romelia Senki - 01 (2026) [SubsPlease]");
        assert!(
            !DupeChecker::is_same_release(&erai_romelia, &subsplease_romelia),
            "Erai-raws and SubsPlease releases must NOT be considered duplicates"
        );

        // Leading '#' from tracker table badges
        let erai_zombie = parse_anime_release(
            "[Erai-raws] Zombie Sagashitemasu - 01 [1080p CR WEB-DL AVC AAC][MultiSub][231C13D4].mkv",
        );
        let subsplease_zombie = parse_anime_release("#Zombie Sagashitemasu - 01 (2026) [SubsPlease]");
        assert!(
            !DupeChecker::is_same_release(&erai_zombie, &subsplease_zombie),
            "Erai-raws and SubsPlease releases with '#' prefix must NOT be considered duplicates"
        );

        // HEVC vs AVC video codecs from same group must NOT be considered duplicates
        let erai_hevc = parse_anime_release(
            "[Erai-raws] Tensei shita Daiseijo wa, Seijo de Aru Koto wo Hitakakusu - 01 [1080p CR WEBRip HEVC AAC][MultiSub][842DB762].mkv",
        );
        let erai_avc = parse_anime_release(
            "[Erai-raws] Tensei shita Daiseijo wa Seijo de Aru Koto wo Hitakakusu - 01 [1080p CR WEB-DL AVC AAC][MultiSub][3FD480A0].mkv",
        );
        assert!(
            !DupeChecker::is_same_release(&erai_hevc, &erai_avc),
            "HEVC and AVC releases from same group must NOT be duplicates (coexistence permitted)"
        );

        // True duplicate: Same group, same episode, same codec
        let erai_hevc_tracker = parse_anime_release(
            "Romelia Senki - 01 (2026) [Erai-raws] [HEVC]",
        );
        assert!(
            DupeChecker::is_same_release(&erai_romelia, &erai_hevc_tracker),
            "Same group, episode, and codec must match as duplicate"
        );
    }

    #[tokio::test]
    async fn test_check_ongoing_catchup_rule() -> Result<()> {
        use tempfile::tempdir;
        let dir = tempdir()?;
        let db = Database::open(dir.path().join("dupe_test.db"))?;

        let mut config = AppConfig::default();
        config.animez.enforce_ongoing_catchup = true;
        // Don't check live tracker in unit test
        config.animez.username = "".to_string();

        let flaresolverr = crate::flaresolverr::FlareSolverrClient::new(config.clone())?;
        let animez = AnimezClient::new(config.clone(), flaresolverr)?;
        let prowlarr = ProwlarrClient::new(config.clone())?;
        let checker = DupeChecker::new(config, prowlarr, animez);

        // Episode 1 is always allowed
        let ep1 = parse_anime_release("[SubsPlease] Frieren - 01 (1080p).mkv");
        let res1 = checker.check_ongoing_catchup_rule(&ep1, Some(100), &db).await?;
        assert!(res1.is_none(), "Episode 1 should always be allowed");

        // Batches are always allowed
        let batch = parse_anime_release("[SubsPlease] Frieren - 01-06 (1080p) (Batch).mkv");
        let res_batch = checker.check_ongoing_catchup_rule(&batch, Some(100), &db).await?;
        assert!(res_batch.is_none(), "Batches should always be allowed");

        // Episode 7 without prior sequence is rejected by catch-up rule
        let ep7 = parse_anime_release("[SubsPlease] Frieren - 07 (1080p).mkv");
        let res7 = checker.check_ongoing_catchup_rule(&ep7, Some(100), &db).await?;
        assert!(res7.is_some(), "Episode 7 without prior sequence must be skipped");
        assert!(res7.unwrap().contains("catch-up violation"));

        // Insert Ep 1 as SEEDING in DB
        let disc = crate::models::DiscoveredRelease {
            guid: "ep1-guid".to_string(),
            title: ep1.raw_title.clone(),
            indexer_id: 1,
            indexer_name: "Nyaa".to_string(),
            download_url: None,
            magnet_url: None,
            info_hash: None,
            size_bytes: 1000,
            seeders: 10,
            leechers: 0,
            publish_date: None,
        };
        let id = db.insert_discovered(&disc, &ep1)?;
        db.set_dupe_checked(id, Some(100))?;
        db.set_seeding(id, Some("https://animez.to/torrents/100"))?;

        // Now Episode 7 is allowed because the uploader established prior sequence in DB
        let res7_after = checker.check_ongoing_catchup_rule(&ep7, Some(100), &db).await?;
        assert!(res7_after.is_none(), "Episode 7 must be allowed once prior sequence is in DB");

        Ok(())
    }
}
