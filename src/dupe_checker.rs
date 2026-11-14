use crate::animez_client::{AnimezClient, TrackerTorrentEntry};
use crate::config::AppConfig;
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

    pub fn find_matching_entry(
        candidate: &ParsedRelease,
        existing_entries: &[TrackerTorrentEntry],
    ) -> Option<TrackerTorrentEntry> {
        for entry in existing_entries {
            let existing_parsed = parse_anime_release(&entry.title);
            if Self::is_same_release(candidate, &existing_parsed) {
                return Some(entry.clone());
            }
        }
        None
    }

    /// Compares two parsed releases to determine if they represent the same release
    /// (same anime title + season/episode/pack + resolution + release group).
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

        // Compare release group if both have one
        if let (Some(ga), Some(gb)) = (&a.release_group, &b.release_group) {
            if !ga.eq_ignore_ascii_case(gb) {
                return false;
            }
        }

        true
    }
}

fn normalize_alnum(s: &str) -> String {
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
    }
}
