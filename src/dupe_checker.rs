use crate::animez_client::{AnimezClient, TrackerTorrentEntry};
use crate::config::AppConfig;
use crate::db::Database;
use crate::models::{DupeCheckResult, ParsedRelease};
use crate::parser::parse_anime_release;
use crate::prowlarr::ProwlarrClient;
use crate::qbittorrent::QbittorrentClient;
use anyhow::Result;
use tracing::{debug, info, warn};

pub struct DupeChecker {
    config: AppConfig,
    prowlarr: ProwlarrClient,
    animez: AnimezClient,
    qbit: QbittorrentClient,
}

impl DupeChecker {
    pub fn new(
        config: AppConfig,
        prowlarr: ProwlarrClient,
        animez: AnimezClient,
        qbit: QbittorrentClient,
    ) -> Self {
        Self {
            config,
            prowlarr,
            animez,
            qbit,
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

        // 1. Check if this is a batch/pack
        if parsed.is_season_pack {
            // Check if there is an episode range in the title (e.g. "01-07" or "02-07")
            if let Some((start, end)) = crate::parser::parse_batch_episode_range(&parsed.raw_title)
            {
                if start <= 1 {
                    // Batch starts from Episode 01 (e.g. 01-07 or 01~12)
                    // This satisfies the catch-up batch requirement under AnimeZ Section 3
                    info!(
                        title = %parsed.raw_title,
                        range = %format!("{start:02}-{end:02}"),
                        "Batch starts at episode 01; valid catch-up batch satisfying ongoing rule"
                    );
                    return Ok(None);
                }
                // Batch starts at episode > 1 (e.g. 02-07)!
                // A mid-season batch cannot bypass catch-up unless prior sequence is already established!
            } else if parsed.episode.is_none() || parsed.episode.unwrap_or(0) <= 1 {
                // Complete season pack or movie without mid-season episode numbering
                return Ok(None);
            }
        }

        let ep = match parsed.episode {
            Some(e) => e,
            None => {
                if !parsed.is_season_pack {
                    return Ok(None);
                }
                // Batch with unknown episode range starting mid-season
                2
            }
        };

        // Episode 1 (or 0) starts a sequence, always permitted for single episodes
        if ep <= 1 && !parsed.is_season_pack {
            return Ok(None);
        }

        let target_season = parsed.season.unwrap_or(1);

        // 2. Check episode sequence coverage starting from episode 01 across qBittorrent and local DB
        let coverage = self
            .get_seeded_sequence_coverage(&parsed.clean_title, target_season, mal_id, db)
            .await?;

        if coverage >= ep - 1 {
            info!(
                title = %parsed.clean_title,
                season = target_season,
                episode = ep,
                coverage = coverage,
                "Ongoing sequence verified starting from episode 01 up to at least {}; next episode permitted",
                ep - 1
            );
            return Ok(None);
        }

        // 3. Check AnimeZ tracker profile uploads (with season isolation)
        let username = self.config.animez.username.trim();
        if !username.is_empty()
            && !username.starts_with("REPLACE_")
            && !username.starts_with("your_")
        {
            match self
                .animez
                .has_user_uploaded_prior_episode(username, &parsed.clean_title, parsed.season)
                .await
            {
                Ok(true) => {
                    info!(
                        title = %parsed.clean_title,
                        season = target_season,
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

        let reason = if parsed.is_season_pack {
            if let Some((start, end)) = crate::parser::parse_batch_episode_range(&parsed.raw_title)
            {
                format!(
                    "AnimeZ upload rules (Section 3) catch-up violation: batch starts at episode {start} > 1 without prior sequence (episodes 01~{end}); catch-up batch must start from episode 01"
                )
            } else {
                "AnimeZ upload rules (Section 3) catch-up violation: mid-season batch without prior sequence; upload a catch-up batch of all available episodes starting from episode 01".to_string()
            }
        } else {
            format!(
                "AnimeZ upload rules (Section 3) catch-up violation: episode {ep} > 1 cannot be uploaded mid-season without prior sequence (episodes 01~{ep}); upload a catch-up batch of all available episodes first"
            )
        };
        Ok(Some(reason))
    }

    /// Returns the maximum contiguous episode number seeded starting from episode 01 for this anime and season
    /// across both active qBittorrent torrents and the local SQLite database.
    pub async fn get_seeded_sequence_coverage(
        &self,
        clean_title: &str,
        season: u32,
        mal_id: Option<u64>,
        db: &Database,
    ) -> Result<u32> {
        let mut qbit_covered = 0;
        let mut qbit_single_eps = std::collections::HashSet::new();

        if let Ok(seeding_torrents) = self
            .qbit
            .list_torrents(Some(&self.config.qbittorrent.seeding_category))
            .await
        {
            let norm_target = crate::parser::normalize_title(clean_title);
            for st in seeding_torrents {
                let st_parsed = parse_anime_release(&st.name);
                let st_season = st_parsed.season.unwrap_or(1);
                if st_season != season {
                    continue;
                }
                let norm_st = crate::parser::normalize_title(&st_parsed.clean_title);
                if norm_st == norm_target
                    || norm_st.starts_with(&norm_target)
                    || norm_target.starts_with(&norm_st)
                {
                    if let Some((start, end)) = crate::parser::parse_batch_episode_range(&st.name)
                        .or_else(|| crate::parser::parse_batch_episode_range(&st_parsed.raw_title))
                    {
                        if start <= 1 {
                            qbit_covered = qbit_covered.max(end);
                        }
                    } else if let Some(ep) = st_parsed.episode {
                        qbit_single_eps.insert(ep);
                    }
                }
            }
        }

        while qbit_single_eps.contains(&(qbit_covered + 1)) {
            qbit_covered += 1;
        }

        let db_clone = db.clone();
        let title_owned = clean_title.to_string();
        let db_covered = tokio::task::spawn_blocking(move || {
            db_clone.get_seeded_episode_coverage(&title_owned, season, mal_id)
        })
        .await??;

        Ok(qbit_covered.max(db_covered))
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
        let candidate =
            parse_anime_release("[SubsPlease] Sousou no Frieren - 12 (1080p) [ABCDEF12].mkv");

        let same_diff_crc =
            parse_anime_release("[SubsPlease] Sousou no Frieren - S01E12 (1080p) [99999999].mkv");
        assert!(DupeChecker::is_same_release(&candidate, &same_diff_crc));

        let diff_ep =
            parse_anime_release("[SubsPlease] Sousou no Frieren - 13 (1080p) [ABCDEF12].mkv");
        assert!(!DupeChecker::is_same_release(&candidate, &diff_ep));

        let diff_res =
            parse_anime_release("[SubsPlease] Sousou no Frieren - 12 (720p) [ABCDEF12].mkv");
        assert!(!DupeChecker::is_same_release(&candidate, &diff_res));

        let diff_group =
            parse_anime_release("[Erai-raws] Sousou no Frieren - 12 (1080p) [ABCDEF12].mkv");
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
        let subsplease_zombie =
            parse_anime_release("#Zombie Sagashitemasu - 01 (2026) [SubsPlease]");
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
        let erai_hevc_tracker = parse_anime_release("Romelia Senki - 01 (2026) [Erai-raws] [HEVC]");
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
        let qbit = QbittorrentClient::new(config.clone())?;
        let checker = DupeChecker::new(config, prowlarr, animez, qbit);

        // Episode 1 is always allowed
        let ep1 = parse_anime_release("[SubsPlease] Frieren - 01 (1080p).mkv");
        let res1 = checker
            .check_ongoing_catchup_rule(&ep1, Some(100), &db)
            .await?;
        assert!(res1.is_none(), "Episode 1 should always be allowed");

        // Batches starting at 01 are allowed (valid catch-up batch)
        let batch_01_06 = parse_anime_release("[SubsPlease] Frieren - 01-06 (1080p) (Batch).mkv");
        let res_batch_01 = checker
            .check_ongoing_catchup_rule(&batch_01_06, Some(100), &db)
            .await?;
        assert!(
            res_batch_01.is_none(),
            "Batch starting at episode 01 must be allowed"
        );

        // Batches starting at episode > 1 (e.g. 02-07) are NOT allowed without prior sequence!
        let bad_batch = parse_anime_release("[Erai-raws] Ao Ashi Season 2 - 02-07 [1080p CR WEB-DL AVC AAC][MultiSub][68D14CA1].mkv");
        let res_bad_batch = checker
            .check_ongoing_catchup_rule(&bad_batch, Some(200), &db)
            .await?;
        assert!(
            res_bad_batch.is_some(),
            "Batch starting at episode 2 without prior sequence must be skipped"
        );
        assert!(res_bad_batch
            .unwrap()
            .contains("batch starts at episode 2 > 1"));

        // Batch of the same series starting at episode 01 IS allowed
        let good_batch = parse_anime_release("[Erai-raws] Ao Ashi Season 2 - 01-07 [1080p CR WEB-DL AVC AAC][MultiSub][68D14CA1].mkv");
        let res_good_batch = checker
            .check_ongoing_catchup_rule(&good_batch, Some(200), &db)
            .await?;
        assert!(
            res_good_batch.is_none(),
            "Batch starting at episode 01 must be allowed as a catch-up batch"
        );

        // Episode 7 without prior sequence is rejected by catch-up rule
        let ep7 = parse_anime_release("[SubsPlease] Frieren - 07 (1080p).mkv");
        let res7 = checker
            .check_ongoing_catchup_rule(&ep7, Some(100), &db)
            .await?;
        assert!(
            res7.is_some(),
            "Episode 7 without prior sequence must be skipped"
        );
        assert!(res7.unwrap().contains("catch-up violation"));

        // Insert Ep 1 (Season 1) as SEEDING in DB
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
            info_url: None,
        };
        let id = db.insert_discovered(&disc, &ep1)?;
        db.set_dupe_checked(id, Some(100))?;
        db.set_seeding(id, Some("https://animez.to/torrents/100"))?;

        // With only Episode 1 seeded, Episode 2 IS allowed as the immediate next episode
        let ep2 = parse_anime_release("[SubsPlease] Frieren - 02 (1080p).mkv");
        let res2 = checker
            .check_ongoing_catchup_rule(&ep2, Some(100), &db)
            .await?;
        assert!(
            res2.is_none(),
            "Episode 2 must be allowed once Episode 1 is seeded"
        );

        // But Episode 7 is STILL REJECTED because episodes 2-6 are missing!
        let res7_gap = checker
            .check_ongoing_catchup_rule(&ep7, Some(100), &db)
            .await?;
        assert!(
            res7_gap.is_some(),
            "Episode 7 must be rejected when episodes 2-6 are missing"
        );

        // Now if a catch-up batch 01-06 is seeded in DB:
        let disc_batch = crate::models::DiscoveredRelease {
            guid: "batch-01-06-guid".to_string(),
            title: batch_01_06.raw_title.clone(),
            indexer_id: 1,
            indexer_name: "Nyaa".to_string(),
            download_url: None,
            magnet_url: None,
            info_hash: None,
            size_bytes: 6000,
            seeders: 10,
            leechers: 0,
            publish_date: None,
            info_url: None,
        };
        let b_id = db.insert_discovered(&disc_batch, &batch_01_06)?;
        db.set_dupe_checked(b_id, Some(100))?;
        db.set_seeding(b_id, Some("https://animez.to/torrents/101"))?;

        // Now Season 1 Episode 7 IS allowed because batch 01-06 covers up to episode 6!
        let res7_after = checker
            .check_ongoing_catchup_rule(&ep7, Some(100), &db)
            .await?;
        assert!(
            res7_after.is_none(),
            "Season 1 Episode 7 must be allowed once batch 01-06 is seeded"
        );

        // BUT Season 2 Episode 7 must STILL BE REJECTED because Season 1 does NOT satisfy Season 2!
        let s2_ep7 = parse_anime_release("[SubsPlease] Frieren Season 2 - 07 (1080p).mkv");
        let res_s2_ep7 = checker
            .check_ongoing_catchup_rule(&s2_ep7, Some(100), &db)
            .await?;
        assert!(
            res_s2_ep7.is_some(),
            "Season 2 Episode 7 must be rejected even if Season 1 was seeded"
        );
        assert!(res_s2_ep7.unwrap().contains("catch-up violation"));

        Ok(())
    }
}
