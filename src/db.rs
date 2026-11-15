use crate::models::{DiscoveredRelease, MalMetadata, PackageBundle, ParsedRelease, ReleaseRecord, ReleaseState};
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Database {
    db_path: PathBuf,
}

impl Database {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let db = Self { db_path };
        db.init_schema()?;
        Ok(db)
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path).with_context(|| {
            format!("Failed to open SQLite database at {}", self.db_path.display())
        })?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(conn)
    }

    pub fn init_schema(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS releases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                guid TEXT NOT NULL UNIQUE,
                raw_title TEXT NOT NULL,
                normalized_key TEXT NOT NULL,
                parsed_json TEXT NOT NULL,
                indexer_id INTEGER NOT NULL,
                indexer_name TEXT NOT NULL,
                download_url TEXT,
                magnet_url TEXT,
                source_info_hash TEXT,
                tracker_info_hash TEXT,
                mal_id INTEGER,
                size_bytes INTEGER NOT NULL,
                seeders INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                save_path TEXT,
                content_path TEXT,
                packaged_torrent_path TEXT,
                mediainfo_path TEXT,
                screenshots_json TEXT,
                tracker_torrent_url TEXT,
                error_message TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_releases_state ON releases(state);
            CREATE INDEX IF NOT EXISTS idx_releases_norm_key ON releases(normalized_key);
            CREATE INDEX IF NOT EXISTS idx_releases_info_hash ON releases(source_info_hash);

            CREATE TABLE IF NOT EXISTS mal_cache (
                search_key TEXT PRIMARY KEY,
                mal_id INTEGER NOT NULL,
                metadata_json TEXT NOT NULL,
                cached_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Checks whether a release is already tracked by GUID, info_hash, or normalized_key.
    pub fn is_duplicate_in_db(
        &self,
        guid: &str,
        info_hash: Option<&str>,
        normalized_key: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;

        let by_guid: Option<i64> = conn
            .query_row(
                "SELECT id FROM releases WHERE guid = ?1 LIMIT 1",
                params![guid],
                |r| r.get(0),
            )
            .optional()?;
        if by_guid.is_some() {
            return Ok(true);
        }

        if let Some(hash) = info_hash {
            if !hash.trim().is_empty() {
                let by_hash: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM releases WHERE LOWER(source_info_hash) = LOWER(?1) LIMIT 1",
                        params![hash.trim()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if by_hash.is_some() {
                    return Ok(true);
                }
            }
        }

        if !normalized_key.trim().is_empty() {
            let by_norm: Option<i64> = conn
                .query_row(
                    "SELECT id FROM releases WHERE normalized_key = ?1 AND state != 'FAILED' LIMIT 1",
                    params![normalized_key],
                    |r| r.get(0),
                )
                .optional()?;
            if by_norm.is_some() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Inserts a newly discovered release in `DISCOVERED` state.
    pub fn insert_discovered(
        &self,
        item: &DiscoveredRelease,
        parsed: &ParsedRelease,
    ) -> Result<i64> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        let parsed_json = serde_json::to_string(parsed)?;

        conn.execute(
            r#"
            INSERT INTO releases (
                guid, raw_title, normalized_key, parsed_json,
                indexer_id, indexer_name, download_url, magnet_url,
                source_info_hash, size_bytes, seeders, state,
                created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            "#,
            params![
                item.guid,
                item.title,
                parsed.normalized_key,
                parsed_json,
                item.indexer_id,
                item.indexer_name,
                item.download_url,
                item.magnet_url,
                item.info_hash,
                item.size_bytes as i64,
                item.seeders,
                ReleaseState::Discovered.as_str(),
                now,
                now,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    pub fn update_state(&self, id: i64, state: ReleaseState) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, error_message = NULL, updated_at = ?2 WHERE id = ?3",
            params![state.as_str(), now, id],
        )?;
        Ok(())
    }

    pub fn set_dupe_checked(&self, id: i64, mal_id: Option<u64>) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, mal_id = COALESCE(?2, mal_id), error_message = NULL, updated_at = ?3 WHERE id = ?4",
            params![
                ReleaseState::DupeChecked.as_str(),
                mal_id.map(|v| v as i64),
                now,
                id
            ],
        )?;
        Ok(())
    }

    pub fn mark_skipped(&self, id: i64, reason: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, error_message = ?2, updated_at = ?3 WHERE id = ?4",
            params![ReleaseState::Skipped.as_str(), reason, now, id],
        )?;
        Ok(())
    }

    pub fn mark_failed(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, error_message = ?2, retry_count = retry_count + 1, updated_at = ?3 WHERE id = ?4",
            params![ReleaseState::Failed.as_str(), error, now, id],
        )?;
        Ok(())
    }

    pub fn set_downloading(&self, id: i64, source_info_hash: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, source_info_hash = ?2, error_message = NULL, updated_at = ?3 WHERE id = ?4",
            params![
                ReleaseState::Downloading.as_str(),
                source_info_hash.to_ascii_lowercase(),
                now,
                id
            ],
        )?;
        Ok(())
    }

    pub fn set_download_completed(
        &self,
        id: i64,
        save_path: &str,
        content_path: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, save_path = ?2, content_path = ?3, error_message = NULL, updated_at = ?4 WHERE id = ?5",
            params![
                ReleaseState::Packaging.as_str(),
                save_path,
                content_path,
                now,
                id
            ],
        )?;
        Ok(())
    }

    pub fn set_packaged(&self, id: i64, bundle: &PackageBundle) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        let screenshots: Vec<String> = bundle
            .screenshots
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let screenshots_json = serde_json::to_string(&screenshots)?;

        conn.execute(
            r#"
            UPDATE releases
            SET state = ?1,
                tracker_info_hash = ?2,
                packaged_torrent_path = ?3,
                mediainfo_path = ?4,
                screenshots_json = ?5,
                error_message = NULL,
                updated_at = ?6
            WHERE id = ?7
            "#,
            params![
                ReleaseState::Uploading.as_str(),
                bundle.torrent_info_hash,
                bundle.torrent_path.to_string_lossy().to_string(),
                bundle.mediainfo_path.to_string_lossy().to_string(),
                screenshots_json,
                now,
                id
            ],
        )?;
        Ok(())
    }

    pub fn set_seeding(&self, id: i64, tracker_torrent_url: Option<&str>) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE releases SET state = ?1, tracker_torrent_url = ?2, error_message = NULL, updated_at = ?3 WHERE id = ?4",
            params![ReleaseState::Seeding.as_str(), tracker_torrent_url, now, id],
        )?;
        Ok(())
    }

    pub fn get_release_by_id(&self, id: i64) -> Result<Option<ReleaseRecord>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT * FROM releases WHERE id = ?1",
            params![id],
            Self::map_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_releases(
        &self,
        state_filter: Option<ReleaseState>,
        limit: usize,
    ) -> Result<Vec<ReleaseRecord>> {
        let conn = self.connect()?;
        let mut records = Vec::new();

        if let Some(st) = state_filter {
            let mut stmt = conn.prepare(
                "SELECT * FROM releases WHERE state = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![st.as_str(), limit as i64], Self::map_row)?;
            for row in rows {
                records.push(row?);
            }
        } else {
            let mut stmt =
                conn.prepare("SELECT * FROM releases ORDER BY id DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit as i64], Self::map_row)?;
            for row in rows {
                records.push(row?);
            }
        }

        Ok(records)
    }

    /// Fetch releases in non-terminal states so the pipeline can resume or advance them.
    pub fn get_actionable_releases(&self, limit: usize) -> Result<Vec<ReleaseRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT * FROM releases
            WHERE state IN ('DISCOVERED', 'DUPE_CHECKED', 'DOWNLOADING', 'PACKAGING', 'UPLOADING')
            ORDER BY
                CASE state
                    WHEN 'UPLOADING' THEN 1
                    WHEN 'PACKAGING' THEN 2
                    WHEN 'DOWNLOADING' THEN 3
                    WHEN 'DUPE_CHECKED' THEN 4
                    WHEN 'DISCOVERED' THEN 5
                    ELSE 6
                END ASC,
                id ASC
            "#,
        )?;
        let rows = stmt.query_map([], Self::map_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }

        // Sort so that active in-flight states come first, and for DISCOVERED,
        // batches and lower episode numbers come before higher episode numbers:
        out.sort_by_key(|r| {
            let state_rank = match r.state {
                ReleaseState::Uploading => 1,
                ReleaseState::Packaging => 2,
                ReleaseState::Downloading => 3,
                ReleaseState::DupeChecked => 4,
                ReleaseState::Discovered => 5,
                _ => 6,
            };
            let parsed = r.parsed().ok();
            let is_pack = parsed.as_ref().map(|p| p.is_season_pack).unwrap_or(false);
            let ep = parsed.as_ref().and_then(|p| p.episode).unwrap_or(u32::MAX);
            (state_rank, if is_pack { 0 } else { 1 }, ep, r.id)
        });

        out.truncate(limit);
        Ok(out)
    }

    pub fn get_release_by_normalized_key(&self, key: &str) -> Result<Option<ReleaseRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT * FROM releases WHERE normalized_key = ?1 LIMIT 1")?;
        let mut rows = stmt.query_map(params![key], Self::map_row)?;
        if let Some(r) = rows.next() {
            Ok(Some(r?))
        } else {
            Ok(None)
        }
    }

    /// Resets a FAILED or SKIPPED release back to the appropriate stage for retrying.
    pub fn reset_for_retry(&self, id: i64) -> Result<ReleaseState> {
        let rec = self
            .get_release_by_id(id)?
            .with_context(|| format!("No release found with ID {id}"))?;

        let target_state = if rec.packaged_torrent_path.is_some() && rec.mediainfo_path.is_some() {
            ReleaseState::Uploading
        } else if rec.content_path.is_some() && rec.save_path.is_some() {
            ReleaseState::Packaging
        } else if rec.source_info_hash.is_some() && rec.state == ReleaseState::Failed {
            ReleaseState::DupeChecked
        } else {
            ReleaseState::Discovered
        };

        self.update_state(id, target_state)?;
        Ok(target_state)
    }

    pub fn get_cached_mal(&self, search_key: &str, ttl_days: u32) -> Result<Option<MalMetadata>> {
        let conn = self.connect()?;
        let key = search_key.trim().to_ascii_lowercase();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT metadata_json, cached_at FROM mal_cache WHERE search_key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if let Some((json_str, cached_at_str)) = row {
            if let Ok(cached_at) = chrono::DateTime::parse_from_rfc3339(&cached_at_str) {
                let age = Utc::now().signed_duration_since(cached_at.with_timezone(&Utc));
                if age <= Duration::days(ttl_days as i64) {
                    let meta: MalMetadata = serde_json::from_str(&json_str)?;
                    return Ok(Some(meta));
                }
            }
        }
        Ok(None)
    }

    pub fn set_cached_mal(&self, search_key: &str, meta: &MalMetadata) -> Result<()> {
        let conn = self.connect()?;
        let key = search_key.trim().to_ascii_lowercase();
        let json_str = serde_json::to_string(meta)?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            r#"
            INSERT INTO mal_cache (search_key, mal_id, metadata_json, cached_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(search_key) DO UPDATE SET
                mal_id = excluded.mal_id,
                metadata_json = excluded.metadata_json,
                cached_at = excluded.cached_at
            "#,
            params![key, meta.mal_id as i64, json_str, now],
        )?;
        Ok(())
    }

    /// Checks if SQLite already tracks a successfully uploaded / seeding release for this anime.
    pub fn has_prior_upload_for_anime(
        &self,
        clean_title: &str,
        mal_id: Option<u64>,
        season: Option<u32>,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let norm_prefix: String = clean_title
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");

        let s = season.unwrap_or(1);
        let season_token = format!("|s{s:02}");

        // 1. Check by MAL ID if available
        if let Some(mid) = mal_id {
            let by_mal: Option<i64> = conn
                .query_row(
                    "SELECT id FROM releases WHERE state = 'SEEDING' AND mal_id = ?1 LIMIT 1",
                    params![mid as i64],
                    |r| r.get(0),
                )
                .optional()?;
            if by_mal.is_some() {
                return Ok(true);
            }
        }

        // 2. Check by normalized title & season in normalized_key
        if !norm_prefix.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT id, normalized_key FROM releases WHERE state = 'SEEDING' AND normalized_key LIKE ?1 LIMIT 10",
            )?;
            let like_pat = format!("{norm_prefix}%");
            let rows = stmt.query_map(params![like_pat], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (_, key) = row?;
                if key.contains(&season_token) || key.starts_with(&norm_prefix) {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// Un-skips any releases for an anime that were previously postponed due to the ongoing catch-up rule.
    pub fn unskip_catchup_releases_for_anime(
        &self,
        clean_title: &str,
        mal_id: Option<u64>,
    ) -> Result<usize> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();

        let norm_prefix: String = clean_title
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");

        let mut total_unskipped = 0;

        // 1. Un-skip by MAL ID if present
        if let Some(mid) = mal_id {
            let count = conn.execute(
                r#"
                UPDATE releases
                SET state = 'DISCOVERED',
                    error_message = NULL,
                    updated_at = ?1
                WHERE state = 'SKIPPED'
                  AND error_message LIKE '%catch-up%'
                  AND mal_id = ?2
                "#,
                params![now, mid as i64],
            )?;
            total_unskipped += count;
        }

        // 2. Un-skip by normalized title prefix if present
        if !norm_prefix.is_empty() {
            let like_pat = format!("{norm_prefix}%");
            let count = conn.execute(
                r#"
                UPDATE releases
                SET state = 'DISCOVERED',
                    error_message = NULL,
                    updated_at = ?1
                WHERE state = 'SKIPPED'
                  AND error_message LIKE '%catch-up%'
                  AND normalized_key LIKE ?2
                "#,
                params![now, like_pat],
            )?;
            total_unskipped += count;
        }

        Ok(total_unskipped)
    }

    /// Finds the earliest actionable release (e.g. Episode 01 or batch) for an anime.
    pub fn find_earliest_actionable_release_for_anime(
        &self,
        clean_title: &str,
        mal_id: Option<u64>,
    ) -> Result<Option<ReleaseRecord>> {
        let conn = self.connect()?;
        let norm_prefix: String = clean_title
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");

        let mut candidates: Vec<ReleaseRecord> = Vec::new();

        // 1. Check by MAL ID if present
        if let Some(mid) = mal_id {
            let mut stmt = conn.prepare(
                r#"
                SELECT * FROM releases
                WHERE state IN ('DISCOVERED', 'DUPE_CHECKED', 'DOWNLOADING', 'PACKAGING', 'UPLOADING')
                  AND mal_id = ?1
                "#,
            )?;
            let rows = stmt.query_map(params![mid as i64], Self::map_row)?;
            for r in rows {
                candidates.push(r?);
            }
        }

        // 2. Check by normalized prefix if empty
        if candidates.is_empty() && !norm_prefix.is_empty() {
            let like_pat = format!("{norm_prefix}%");
            let mut stmt = conn.prepare(
                r#"
                SELECT * FROM releases
                WHERE state IN ('DISCOVERED', 'DUPE_CHECKED', 'DOWNLOADING', 'PACKAGING', 'UPLOADING')
                  AND normalized_key LIKE ?1
                "#,
            )?;
            let rows = stmt.query_map(params![like_pat], Self::map_row)?;
            for r in rows {
                candidates.push(r?);
            }
        }

        if candidates.is_empty() {
            return Ok(None);
        }

        // Sort candidates so the true earliest release comes first:
        // 1. Season packs / batches come first (is_season_pack == true -> 0, else 1)
        // 2. Lowest episode number (1, 2, 3...)
        // 3. Furthest pipeline stage (UPLOADING > PACKAGING > DOWNLOADING > DUPE_CHECKED > DISCOVERED)
        // 4. Lowest id
        candidates.sort_by_key(|r| {
            let parsed = r.parsed().ok();
            let is_pack = parsed.as_ref().map(|p| p.is_season_pack).unwrap_or(false);
            let ep = parsed.as_ref().and_then(|p| p.episode).unwrap_or(u32::MAX);
            let state_rank = match r.state {
                ReleaseState::Uploading => 1,
                ReleaseState::Packaging => 2,
                ReleaseState::Downloading => 3,
                ReleaseState::DupeChecked => 4,
                ReleaseState::Discovered => 5,
                _ => 6,
            };
            (if is_pack { 0 } else { 1 }, ep, state_rank, r.id)
        });

        Ok(candidates.into_iter().next())
    }

    /// If no actionable release is pending, checks if a starter (batch or episode <= 1)
    /// was previously SKIPPED and un-skips it back to DISCOVERED state.
    pub fn find_and_unskip_starter_for_anime(
        &self,
        clean_title: &str,
        mal_id: Option<u64>,
    ) -> Result<Option<ReleaseRecord>> {
        let conn = self.connect()?;
        let norm_prefix: String = clean_title
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");

        let mut candidates: Vec<ReleaseRecord> = Vec::new();
        if let Some(mid) = mal_id {
            let mut stmt = conn.prepare(
                r#"
                SELECT * FROM releases
                WHERE state IN ('SKIPPED', 'FAILED')
                  AND mal_id = ?1
                "#,
            )?;
            let rows = stmt.query_map(params![mid as i64], Self::map_row)?;
            for r in rows {
                candidates.push(r?);
            }
        }

        if candidates.is_empty() && !norm_prefix.is_empty() {
            let like_pat = format!("{norm_prefix}%");
            let mut stmt = conn.prepare(
                r#"
                SELECT * FROM releases
                WHERE state IN ('SKIPPED', 'FAILED')
                  AND normalized_key LIKE ?1
                "#,
            )?;
            let rows = stmt.query_map(params![like_pat], Self::map_row)?;
            for r in rows {
                candidates.push(r?);
            }
        }

        let mut starters: Vec<ReleaseRecord> = candidates
            .into_iter()
            .filter(|r| {
                if let Ok(p) = r.parsed() {
                    p.is_season_pack || p.episode.is_some_and(|ep| ep <= 1)
                } else {
                    false
                }
            })
            .collect();

        starters.sort_by_key(|r| {
            let parsed = r.parsed().ok();
            let is_pack = parsed.as_ref().map(|p| p.is_season_pack).unwrap_or(false);
            let ep = parsed.as_ref().and_then(|p| p.episode).unwrap_or(u32::MAX);
            (if is_pack { 0 } else { 1 }, ep, r.id)
        });

        if let Some(starter) = starters.into_iter().next() {
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE releases SET state = 'DISCOVERED', error_message = NULL, updated_at = ?1 WHERE id = ?2",
                params![now, starter.id],
            )?;
            let mut unskipped = starter;
            unskipped.state = ReleaseState::Discovered;
            unskipped.error_message = None;
            return Ok(Some(unskipped));
        }

        Ok(None)
    }

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReleaseRecord> {
        let state_str: String = row.get("state")?;
        let state = ReleaseState::from_str(&state_str).unwrap_or(ReleaseState::Failed);
        let size_i64: i64 = row.get("size_bytes")?;
        let mal_id_opt: Option<i64> = row.get("mal_id")?;

        Ok(ReleaseRecord {
            id: row.get("id")?,
            guid: row.get("guid")?,
            raw_title: row.get("raw_title")?,
            normalized_key: row.get("normalized_key")?,
            parsed_json: row.get("parsed_json")?,
            indexer_id: row.get("indexer_id")?,
            indexer_name: row.get("indexer_name")?,
            download_url: row.get("download_url")?,
            magnet_url: row.get("magnet_url")?,
            source_info_hash: row.get("source_info_hash")?,
            tracker_info_hash: row.get("tracker_info_hash")?,
            mal_id: mal_id_opt.map(|v| v as u64),
            size_bytes: size_i64.max(0) as u64,
            seeders: row.get("seeders")?,
            state,
            save_path: row.get("save_path")?,
            content_path: row.get("content_path")?,
            packaged_torrent_path: row.get("packaged_torrent_path")?,
            mediainfo_path: row.get("mediainfo_path")?,
            screenshots_json: row.get("screenshots_json")?,
            tracker_torrent_url: row.get("tracker_torrent_url")?,
            error_message: row.get("error_message")?,
            retry_count: row.get("retry_count")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_db_lifecycle_and_state_machine() -> Result<()> {
        let dir = tempdir()?;
        let db = Database::open(dir.path().join("test.db"))?;

        let disc = DiscoveredRelease {
            guid: "https://nyaa.si/view/123456".to_string(),
            title: "[SubsPlease] Frieren - 01 (1080p) [ABCD1234].mkv".to_string(),
            indexer_id: 1,
            indexer_name: "Nyaa".to_string(),
            download_url: Some("https://nyaa.si/download/123456.torrent".to_string()),
            magnet_url: None,
            info_hash: Some("abcdef1234567890abcdef1234567890abcdef12".to_string()),
            size_bytes: 1_400_000_000,
            seeders: 42,
            leechers: 3,
            publish_date: Some(Utc::now()),
        };

        let parsed = ParsedRelease {
            raw_title: disc.title.clone(),
            clean_title: "Frieren".to_string(),
            normalized_key: "frieren|s01e01|1080p|subsplease".to_string(),
            release_group: Some("SubsPlease".to_string()),
            season: Some(1),
            episode: Some(1),
            resolution: Some("1080p".to_string()),
            ..Default::default()
        };

        assert!(!db.is_duplicate_in_db(
            &disc.guid,
            disc.info_hash.as_deref(),
            &parsed.normalized_key
        )?);

        let id = db.insert_discovered(&disc, &parsed)?;
        assert!(db.is_duplicate_in_db(&disc.guid, None, "")?);
        assert!(db.is_duplicate_in_db("other-guid", disc.info_hash.as_deref(), "")?);
        assert!(db.is_duplicate_in_db("other-guid", None, &parsed.normalized_key)?);

        db.set_dupe_checked(id, Some(52991))?;
        let rec = db.get_release_by_id(id)?.unwrap();
        assert_eq!(rec.state, ReleaseState::DupeChecked);
        assert_eq!(rec.mal_id, Some(52991));

        db.mark_failed(id, "network timeout")?;
        let rec = db.get_release_by_id(id)?.unwrap();
        assert_eq!(rec.state, ReleaseState::Failed);
        assert_eq!(rec.retry_count, 1);

        let next = db.reset_for_retry(id)?;
        assert_eq!(next, ReleaseState::DupeChecked);

        assert!(!db.has_prior_upload_for_anime("Frieren", Some(52991), Some(1))?);
        db.set_seeding(id, Some("https://animez.to/torrents/123"))?;
        assert!(db.has_prior_upload_for_anime("Frieren", Some(52991), Some(1))?);

        // Test un-skipping catch-up releases
        let disc3 = DiscoveredRelease {
            guid: "https://nyaa.si/view/123458".to_string(),
            title: "[SubsPlease] Frieren - 03 (1080p) [ABCD1236].mkv".to_string(),
            indexer_id: 1,
            indexer_name: "Nyaa".to_string(),
            download_url: Some("https://nyaa.si/download/123458.torrent".to_string()),
            magnet_url: None,
            info_hash: Some("abcdef1234567890abcdef1234567890abcdef14".to_string()),
            size_bytes: 1_400_000_000,
            seeders: 50,
            leechers: 2,
            publish_date: Some(Utc::now()),
        };
        let parsed3 = ParsedRelease {
            raw_title: disc3.title.clone(),
            clean_title: "Frieren".to_string(),
            normalized_key: "frieren|s01e03|1080p|subsplease".to_string(),
            release_group: Some("SubsPlease".to_string()),
            season: Some(1),
            episode: Some(3),
            resolution: Some("1080p".to_string()),
            ..Default::default()
        };
        let id3 = db.insert_discovered(&disc3, &parsed3)?;
        db.set_dupe_checked(id3, Some(52991))?;
        db.mark_skipped(id3, "AnimeZ catch-up required: missing prior episodes")?;
        let rec3 = db.get_release_by_id(id3)?.unwrap();
        assert_eq!(rec3.state, ReleaseState::Skipped);

        let unskipped = db.unskip_catchup_releases_for_anime("Frieren", Some(52991))?;
        assert_eq!(unskipped, 1);
        let rec3_unskipped = db.get_release_by_id(id3)?.unwrap();
        assert_eq!(rec3_unskipped.state, ReleaseState::Discovered);
        assert!(rec3_unskipped.error_message.is_none());

        // Test finding earliest actionable release for anime
        let earliest = db.find_earliest_actionable_release_for_anime("Frieren", Some(52991))?;
        assert!(earliest.is_some());
        assert_eq!(earliest.unwrap().id, id3);

        Ok(())
    }
}
