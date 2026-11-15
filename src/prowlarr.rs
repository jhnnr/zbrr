use crate::config::AppConfig;
use crate::models::{DiscoveredRelease, ParsedRelease};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

pub struct ProwlarrClient {
    http: Client,
    config: AppConfig,
    include_regexes: Vec<Regex>,
    exclude_regexes: Vec<Regex>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProwlarrApiSearchItem {
    guid: Option<String>,
    title: Option<String>,
    indexer_id: Option<i64>,
    indexer: Option<String>,
    download_url: Option<String>,
    magnet_url: Option<String>,
    info_hash: Option<String>,
    size: Option<u64>,
    seeders: Option<u32>,
    leechers: Option<u32>,
    publish_date: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProwlarrIndexerInfo {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub definition_name: Option<String>,
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub privacy: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProwlarrSystemStatus {
    version: Option<String>,
    app_name: Option<String>,
}

impl ProwlarrClient {
    pub fn new(config: AppConfig) -> Result<Self> {
        let http = Client::builder()
            .user_agent("zbrr/0.1.0")
            .timeout(Duration::from_secs(config.prowlarr.timeout_secs.max(10)))
            .build()
            .context("Failed to build Prowlarr HTTP client")?;

        let mut include_regexes = Vec::new();
        for pat in &config.filters.include_regex {
            include_regexes.push(
                Regex::new(pat)
                    .with_context(|| format!("Invalid filter include_regex pattern: {pat}"))?,
            );
        }

        let mut exclude_regexes = Vec::new();
        for pat in &config.filters.exclude_regex {
            exclude_regexes.push(
                Regex::new(pat)
                    .with_context(|| format!("Invalid filter exclude_regex pattern: {pat}"))?,
            );
        }

        Ok(Self {
            http,
            config,
            include_regexes,
            exclude_regexes,
        })
    }

    /// Fetches the list of all configured indexers from Prowlarr (`/api/v1/indexer`).
    pub async fn list_indexers(&self) -> Result<Vec<ProwlarrIndexerInfo>> {
        let base = self.config.prowlarr.url.trim_end_matches('/');
        let url = format!("{base}/api/v1/indexer");

        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.config.prowlarr.api_key)
            .send()
            .await
            .with_context(|| format!("Failed to reach Prowlarr at {url}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("Prowlarr /api/v1/indexer returned HTTP {}", resp.status());
        }

        let indexers: Vec<ProwlarrIndexerInfo> = resp
            .json()
            .await
            .context("Failed to deserialize Prowlarr indexers JSON")?;
        Ok(indexers)
    }

    /// Finds the AnimeZ indexer ID from configuration or by inspecting Prowlarr's indexer list.
    pub fn find_animez_indexer_id(&self, indexers: &[ProwlarrIndexerInfo]) -> Option<i64> {
        if self.config.prowlarr.animez_indexer_id > 0 {
            return Some(self.config.prowlarr.animez_indexer_id);
        }
        for idx in indexers {
            let lower = idx.name.to_ascii_lowercase();
            if lower.contains("animez") {
                return Some(idx.id);
            }
            if let Some(def) = &idx.definition_name {
                if def.to_ascii_lowercase().contains("animez") {
                    return Some(idx.id);
                }
            }
        }
        None
    }

    /// Returns the AnimeZ indexer ID, auto-detecting via `/api/v1/indexer` if not set in config.
    pub async fn get_animez_indexer_id(&self) -> Option<i64> {
        if self.config.prowlarr.animez_indexer_id > 0 {
            return Some(self.config.prowlarr.animez_indexer_id);
        }
        if let Ok(indexers) = self.list_indexers().await {
            return self.find_animez_indexer_id(&indexers);
        }
        None
    }

    /// Resolves which Prowlarr indexer IDs to poll based on:
    /// 1. `config.prowlarr.indexer_ids` (explicit numeric IDs)
    /// 2. `config.prowlarr.indexers` (matching name or definition name)
    /// 3. If neither specified: all enabled torrent indexers
    /// 4. Excluding `config.prowlarr.exclude_indexers` and the AnimeZ tracker itself
    pub async fn resolve_source_indexer_ids(&self) -> Result<Vec<i64>> {
        let all_indexers = match self.list_indexers().await {
            Ok(list) => list,
            Err(e) => {
                debug!(error = %e, "Could not fetch /api/v1/indexer; using configured indexer_ids");
                return Ok(self.config.prowlarr.indexer_ids.clone());
            }
        };

        let animez_id = self.find_animez_indexer_id(&all_indexers);

        let is_excluded = |id: i64, name: &str| -> bool {
            if let Some(aid) = animez_id {
                if id == aid {
                    return true;
                }
            }
            for excl in &self.config.prowlarr.exclude_indexers {
                if name.eq_ignore_ascii_case(excl)
                    || name.to_ascii_lowercase().contains(&excl.to_ascii_lowercase())
                {
                    return true;
                }
            }
            false
        };

        let mut target_ids: Vec<i64> = Vec::new();

        // 1. Explicit numeric IDs from config
        for id in &self.config.prowlarr.indexer_ids {
            if let Some(idx) = all_indexers.iter().find(|i| i.id == *id) {
                if !is_excluded(idx.id, &idx.name) && !target_ids.contains(&idx.id) {
                    target_ids.push(idx.id);
                }
            } else if !target_ids.contains(id) {
                target_ids.push(*id);
            }
        }

        // 2. Name-based indexers from config (e.g. ["Nyaa.si", "Nyaa"])
        for name_query in &self.config.prowlarr.indexers {
            let q = name_query.trim().to_ascii_lowercase();
            if q.is_empty() {
                continue;
            }
            for idx in &all_indexers {
                if !idx.enable {
                    continue;
                }
                let matches_name = idx.name.to_ascii_lowercase().contains(&q);
                let matches_def = idx
                    .definition_name
                    .as_deref()
                    .map(|d| d.to_ascii_lowercase().contains(&q))
                    .unwrap_or(false);

                if (matches_name || matches_def)
                    && !is_excluded(idx.id, &idx.name)
                    && !target_ids.contains(&idx.id)
                {
                    target_ids.push(idx.id);
                }
            }
        }

        // 3. If neither was specified, select all enabled indexers
        if self.config.prowlarr.indexer_ids.is_empty() && self.config.prowlarr.indexers.is_empty() {
            for idx in &all_indexers {
                if idx.enable && !is_excluded(idx.id, &idx.name) {
                    target_ids.push(idx.id);
                }
            }
        }

        Ok(target_ids)
    }

    /// Tests connectivity to Prowlarr `/api/v1/system/status` and lists configured indexers.
    pub async fn test_connection(&self) -> Result<String> {
        let base = self.config.prowlarr.url.trim_end_matches('/');
        let url = format!("{base}/api/v1/system/status");

        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.config.prowlarr.api_key)
            .send()
            .await
            .with_context(|| format!("Failed to reach Prowlarr at {url}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("Prowlarr returned HTTP {}", resp.status());
        }

        let status: ProwlarrSystemStatus = resp.json().await.unwrap_or(ProwlarrSystemStatus {
            version: Some("unknown".to_string()),
            app_name: Some("Prowlarr".to_string()),
        });

        let mut summary = format!(
            "{} v{}",
            status.app_name.unwrap_or_else(|| "Prowlarr".to_string()),
            status.version.unwrap_or_else(|| "unknown".to_string())
        );

        if let Ok(indexers) = self.list_indexers().await {
            let names: Vec<String> = indexers
                .iter()
                .map(|i| format!("{} [#{}]", i.name, i.id))
                .collect();
            let source_ids = self.resolve_source_indexer_ids().await.unwrap_or_default();
            let source_names: Vec<String> = indexers
                .iter()
                .filter(|i| source_ids.contains(&i.id))
                .map(|i| format!("{} [#{}]", i.name, i.id))
                .collect();
            let animez_id = self.find_animez_indexer_id(&indexers);
            let animez_str = animez_id
                .map(|id| {
                    let name = indexers
                        .iter()
                        .find(|i| i.id == id)
                        .map(|i| i.name.as_str())
                        .unwrap_or("AnimeZ");
                    format!("{name} [#{id}]")
                })
                .unwrap_or_else(|| "not configured".to_string());

            summary.push_str(&format!(
                "\n         Configured indexers ({}): {}\n         Active source indexers ({}): {}\n         AnimeZ dupe-check indexer: {}",
                indexers.len(),
                names.join(", "),
                source_ids.len(),
                if source_names.is_empty() {
                    "all non-excluded".to_string()
                } else {
                    source_names.join(", ")
                },
                animez_str
            ));
        }

        Ok(summary)
    }

    /// Polls configured Prowlarr indexers via REST API (`/api/v1/search`) or Torznab RSS (`/{id}/api?t=rss`).
    pub async fn poll_releases(&self) -> Result<Vec<DiscoveredRelease>> {
        if self.config.prowlarr.mode.eq_ignore_ascii_case("torznab") {
            self.poll_torznab_rss().await
        } else {
            self.poll_rest_api().await
        }
    }

    async fn poll_rest_api(&self) -> Result<Vec<DiscoveredRelease>> {
        let base = self.config.prowlarr.url.trim_end_matches('/');
        let url = format!("{base}/api/v1/search");

        let target_indexer_ids = self.resolve_source_indexer_ids().await?;

        let mut query_params: Vec<(String, String)> = vec![("type".to_string(), "search".to_string())];
        if !self.config.prowlarr.query.trim().is_empty() {
            query_params.push(("query".to_string(), self.config.prowlarr.query.clone()));
        }
        for id in &target_indexer_ids {
            query_params.push(("indexerIds".to_string(), id.to_string()));
        }
        for cat in &self.config.prowlarr.categories {
            query_params.push(("categories".to_string(), cat.to_string()));
        }

        debug!(
            url = %url,
            target_indexers = ?target_indexer_ids,
            "Polling Prowlarr REST API"
        );
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.config.prowlarr.api_key)
            .query(&query_params)
            .send()
            .await
            .with_context(|| format!("Prowlarr API search failed at {url}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("Prowlarr search returned HTTP {}", resp.status());
        }

        let items: Vec<ProwlarrApiSearchItem> = resp
            .json()
            .await
            .context("Failed to deserialize Prowlarr search JSON")?;

        let mut results = Vec::with_capacity(items.len());
        for item in items {
            let Some(title) = item.title.filter(|t| !t.trim().is_empty()) else {
                continue;
            };

            let indexer_name = item.indexer.unwrap_or_else(|| "Prowlarr".to_string());
            let indexer_id = item.indexer_id.unwrap_or(0);

            // Verify not in excluded indexers
            let is_excluded = self
                .config
                .prowlarr
                .exclude_indexers
                .iter()
                .any(|excl| {
                    indexer_name.eq_ignore_ascii_case(excl)
                        || indexer_name
                            .to_ascii_lowercase()
                            .contains(&excl.to_ascii_lowercase())
                });
            if is_excluded {
                continue;
            }

            // Verify in target_indexer_ids if explicit targets were resolved
            if !target_indexer_ids.is_empty() && indexer_id > 0 && !target_indexer_ids.contains(&indexer_id) {
                continue;
            }

            let guid = item.guid.unwrap_or_else(|| title.clone());
            results.push(DiscoveredRelease {
                guid,
                title,
                indexer_id,
                indexer_name,
                download_url: item.download_url,
                magnet_url: item.magnet_url,
                info_hash: item.info_hash,
                size_bytes: item.size.unwrap_or(0),
                seeders: item.seeders.unwrap_or(0),
                leechers: item.leechers.unwrap_or(0),
                publish_date: item.publish_date,
            });
        }

        info!(count = results.len(), "Fetched candidate items from Prowlarr REST API");
        Ok(results)
    }

    async fn poll_torznab_rss(&self) -> Result<Vec<DiscoveredRelease>> {
        let base = self.config.prowlarr.url.trim_end_matches('/');
        let target_indexer_ids = self.resolve_source_indexer_ids().await?;
        let ids = if target_indexer_ids.is_empty() {
            vec![1]
        } else {
            target_indexer_ids
        };

        let mut all_releases = Vec::new();
        let cats_csv = self
            .config
            .prowlarr
            .categories
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");

        for id in ids {
            let url = format!("{base}/{id}/api");
            let mut params = vec![
                ("t", "rss".to_string()),
                ("apikey", self.config.prowlarr.api_key.clone()),
            ];
            if !cats_csv.is_empty() {
                params.push(("cat", cats_csv.clone()));
            }
            if !self.config.prowlarr.query.trim().is_empty() {
                params.push(("q", self.config.prowlarr.query.clone()));
            }

            let resp = self
                .http
                .get(&url)
                .query(&params)
                .send()
                .await
                .with_context(|| format!("Failed to fetch Torznab RSS from {url}"))?;

            if !resp.status().is_success() {
                anyhow::bail!("Torznab RSS returned HTTP {} for indexer {}", resp.status(), id);
            }

            let xml_text = resp.text().await?;
            let mut parsed = parse_torznab_xml(&xml_text, id)?;
            all_releases.append(&mut parsed);
        }

        info!(
            count = all_releases.len(),
            "Fetched candidate items from Prowlarr Torznab RSS"
        );
        Ok(all_releases)
    }

    /// Searches a specific Prowlarr indexer (e.g. `animez_indexer_id`) for duplicate checking.
    pub async fn search_indexer(
        &self,
        indexer_id: i64,
        query: &str,
    ) -> Result<Vec<DiscoveredRelease>> {
        let base = self.config.prowlarr.url.trim_end_matches('/');
        let url = format!("{base}/api/v1/search");

        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.config.prowlarr.api_key)
            .query(&[
                ("query", query),
                ("indexerIds", &indexer_id.to_string()),
                ("type", "search"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Prowlarr indexer search returned HTTP {}", resp.status());
        }

        let items: Vec<ProwlarrApiSearchItem> = resp.json().await?;
        Ok(items
            .into_iter()
            .filter_map(|i| {
                let title = i.title?;
                Some(DiscoveredRelease {
                    guid: i.guid.unwrap_or_else(|| title.clone()),
                    title,
                    indexer_id: i.indexer_id.unwrap_or(indexer_id),
                    indexer_name: i.indexer.unwrap_or_else(|| "AnimeZ".to_string()),
                    download_url: i.download_url,
                    magnet_url: i.magnet_url,
                    info_hash: i.info_hash,
                    size_bytes: i.size.unwrap_or(0),
                    seeders: i.seeders.unwrap_or(0),
                    leechers: i.leechers.unwrap_or(0),
                    publish_date: i.publish_date,
                })
            })
            .collect())
    }

    /// Builds search queries for missing prior episodes in order of specificity.
    pub fn build_missing_episodes_queries(parsed: &ParsedRelease) -> Vec<String> {
        let group_str = parsed.release_group.as_deref().unwrap_or("").trim();
        let res_str = parsed.resolution.as_deref().unwrap_or("").trim();

        let mut queries = Vec::new();
        // 1. Most specific: Title + Group + Resolution (e.g. "Marronnier Erai-raws 1080p")
        if !group_str.is_empty() && !res_str.is_empty() {
            queries.push(format!("{} {} {}", parsed.clean_title, group_str, res_str));
        }
        // 2. Specific: Title + Group
        if !group_str.is_empty() {
            queries.push(format!("{} {}", parsed.clean_title, group_str));
        }
        // 3. Title alone
        queries.push(parsed.clean_title.clone());
        // 4. Batch query
        if !group_str.is_empty() {
            queries.push(format!("{} {} batch", parsed.clean_title, group_str));
        } else {
            queries.push(format!("{} batch", parsed.clean_title));
        }

        queries
    }

    /// Searches configured Prowlarr source indexers for missing prior episodes or batches of an ongoing anime.
    pub async fn search_missing_prior_episodes(
        &self,
        parsed: &ParsedRelease,
    ) -> Result<Vec<DiscoveredRelease>> {
        let base = self.config.prowlarr.url.trim_end_matches('/');
        let url = format!("{base}/api/v1/search");
        let source_ids = self.resolve_source_indexer_ids().await?;

        let queries = Self::build_missing_episodes_queries(parsed);
        let mut candidates = Vec::new();
        let mut seen_guids = std::collections::HashSet::new();

        for q in queries {
            self.config.polite_sleep().await;
            debug!(query = %q, "Searching Prowlarr source indexers for missing ongoing episodes");

            let mut req = self
                .http
                .get(&url)
                .header("X-Api-Key", &self.config.prowlarr.api_key)
                .query(&[("query", &q), ("type", &"search".to_string())]);

            for id in &source_ids {
                req = req.query(&[("indexerIds", &id.to_string())]);
            }

            let resp = match req.send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    warn!(status = %r.status(), query = %q, "Prowlarr search returned non-success HTTP status");
                    continue;
                }
                Err(e) => {
                    warn!(error = %e, query = %q, "Prowlarr search request failed");
                    continue;
                }
            };

            let items: Vec<ProwlarrApiSearchItem> = resp.json().await.unwrap_or_default();
            for i in items {
                let Some(title) = i.title else { continue };
                let guid = i.guid.unwrap_or_else(|| title.clone());
                if seen_guids.insert(guid.clone()) {
                    candidates.push(DiscoveredRelease {
                        guid,
                        title,
                        indexer_id: i.indexer_id.unwrap_or(0),
                        indexer_name: i.indexer.unwrap_or_default(),
                        download_url: i.download_url,
                        magnet_url: i.magnet_url,
                        info_hash: i.info_hash,
                        size_bytes: i.size.unwrap_or(0),
                        seeders: i.seeders.unwrap_or(0),
                        leechers: i.leechers.unwrap_or(0),
                        publish_date: i.publish_date,
                    });
                }
            }

            // Stop if we found matching releases
            if !candidates.is_empty() {
                break;
            }
        }

        Ok(candidates)
    }

    /// Evaluates configured filter rules (include/exclude regex, group whitelist, resolution, seeders, size).
    pub fn check_filters(
        &self,
        item: &DiscoveredRelease,
        parsed: &ParsedRelease,
    ) -> std::result::Result<(), String> {
        let filters = &self.config.filters;

        // Must have a downloadable link or magnet
        if item.download_url.is_none() && item.magnet_url.is_none() {
            return Err("Neither download_url nor magnet_url present".to_string());
        }

        // Seeders check
        if item.seeders < filters.min_seeders {
            return Err(format!(
                "Seeders ({}) below minimum ({})",
                item.seeders, filters.min_seeders
            ));
        }

        // Size bounds check (if size_bytes > 0)
        if item.size_bytes > 0 {
            if item.size_bytes < filters.min_size_bytes {
                return Err(format!(
                    "Size ({} B) smaller than min_size_bytes ({} B)",
                    item.size_bytes, filters.min_size_bytes
                ));
            }
            if filters.max_size_bytes > 0 && item.size_bytes > filters.max_size_bytes {
                return Err(format!(
                    "Size ({} B) exceeds max_size_bytes ({} B)",
                    item.size_bytes, filters.max_size_bytes
                ));
            }
        }

        // Exclude regexes
        for re in &self.exclude_regexes {
            if re.is_match(&item.title) {
                return Err(format!("Matched exclude_regex: {}", re.as_str()));
            }
        }

        // Include regexes (at least one must match if configured)
        if !self.include_regexes.is_empty()
            && !self.include_regexes.iter().any(|re| re.is_match(&item.title))
        {
            return Err("Did not match any include_regex".to_string());
        }

        // Allowed resolutions
        if !filters.allowed_resolutions.is_empty() {
            let Some(res) = parsed.resolution.as_deref() else {
                return Err("Release has no recognizable resolution".to_string());
            };
            if !filters
                .allowed_resolutions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(res))
            {
                return Err(format!("Resolution '{res}' not in allowed_resolutions"));
            }
        }

        // Allowed release groups
        if !filters.allowed_groups.is_empty() {
            let Some(group) = parsed.release_group.as_deref() else {
                return Err("Release has no recognizable release group".to_string());
            };
            if !filters
                .allowed_groups
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(group))
            {
                return Err(format!("Release group '{group}' not in allowed_groups"));
            }
        }

        // Allowed sources (e.g. WEB-DL, BluRay)
        if !filters.allowed_sources.is_empty() {
            let Some(src) = parsed.source.as_deref() else {
                return Err("Release has no recognizable source (WEB-DL/BluRay/etc)".to_string());
            };
            if !filters
                .allowed_sources
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(src))
            {
                return Err(format!("Source '{src}' not in allowed_sources"));
            }
        }

        // Allowed video codecs (e.g. HEVC, H.264)
        if !filters.allowed_codecs.is_empty() {
            let Some(vc) = parsed.video_codec.as_deref() else {
                return Err("Release has no recognizable video codec".to_string());
            };
            if !filters
                .allowed_codecs
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(vc))
            {
                return Err(format!("Video codec '{vc}' not in allowed_codecs"));
            }
        }

        // Exclude indexers check
        for excl in &self.config.prowlarr.exclude_indexers {
            if item.indexer_name.eq_ignore_ascii_case(excl)
                || item
                    .indexer_name
                    .to_ascii_lowercase()
                    .contains(&excl.to_ascii_lowercase())
            {
                return Err(format!(
                    "Indexer '{}' is in exclude_indexers",
                    item.indexer_name
                ));
            }
        }

        // AnimeZ rule compliance check (container, codecs, resolution, leaks)
        crate::parser::check_animez_rule_compliance(parsed)?;

        Ok(())
    }
}

/// Parses a Torznab XML RSS feed into `Vec<DiscoveredRelease>` using `quick-xml`.
pub fn parse_torznab_xml(xml: &str, default_indexer_id: i64) -> Result<Vec<DiscoveredRelease>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut releases = Vec::new();
    let mut in_item = false;
    let mut current_tag = String::new();

    let mut title = String::new();
    let mut guid = String::new();
    let mut link = String::new();
    let mut size_bytes: u64 = 0;
    let mut seeders: u32 = 0;
    let mut leechers: u32 = 0;
    let mut info_hash: Option<String> = None;
    let mut magnet_url: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag_name == "item" {
                    in_item = true;
                    title.clear();
                    guid.clear();
                    link.clear();
                    size_bytes = 0;
                    seeders = 0;
                    leechers = 0;
                    info_hash = None;
                    magnet_url = None;
                } else if in_item {
                    current_tag = tag_name.clone();
                    if tag_name == "enclosure" {
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref());
                            let val = String::from_utf8_lossy(&attr.value);
                            if key == "url" && link.is_empty() {
                                link = val.to_string();
                            } else if key == "length" && size_bytes == 0 {
                                size_bytes = val.parse().unwrap_or(0);
                            }
                        }
                    } else if tag_name.ends_with("attr") {
                        Self_parse_torznab_attr(&e, &mut seeders, &mut leechers, &mut info_hash, &mut magnet_url, &mut size_bytes);
                    }
                }
            }
            Ok(Event::Empty(e)) if in_item => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag_name == "enclosure" {
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref());
                        let val = String::from_utf8_lossy(&attr.value);
                        if key == "url" && link.is_empty() {
                            link = val.to_string();
                        } else if key == "length" && size_bytes == 0 {
                            size_bytes = val.parse().unwrap_or(0);
                        }
                    }
                } else if tag_name.ends_with("attr") {
                    Self_parse_torznab_attr(&e, &mut seeders, &mut leechers, &mut info_hash, &mut magnet_url, &mut size_bytes);
                }
            }
            Ok(Event::Text(e)) if in_item => {
                let txt = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "title" => title = txt,
                    "guid" => guid = txt,
                    "link" => link = txt,
                    "size" => size_bytes = txt.parse().unwrap_or(size_bytes),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag_name == "item" && in_item {
                    in_item = false;
                    if !title.is_empty() {
                        let final_guid = if guid.is_empty() {
                            title.clone()
                        } else {
                            guid.clone()
                        };
                        let (download_url, final_magnet) = if link.starts_with("magnet:") {
                            (None, Some(link.clone()))
                        } else if !link.is_empty() {
                            (Some(link.clone()), magnet_url.clone())
                        } else {
                            (None, magnet_url.clone())
                        };

                        releases.push(DiscoveredRelease {
                            guid: final_guid,
                            title: title.clone(),
                            indexer_id: default_indexer_id,
                            indexer_name: format!("Torznab-{default_indexer_id}"),
                            download_url,
                            magnet_url: final_magnet,
                            info_hash: info_hash.clone(),
                            size_bytes,
                            seeders,
                            leechers,
                            publish_date: None,
                        });
                    }
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("XML parse error in Torznab feed: {e}"),
            _ => {}
        }
    }

    Ok(releases)
}

#[allow(non_snake_case)]
fn Self_parse_torznab_attr(
    e: &quick_xml::events::BytesStart<'_>,
    seeders: &mut u32,
    leechers: &mut u32,
    info_hash: &mut Option<String>,
    magnet_url: &mut Option<String>,
    size_bytes: &mut u64,
) {
    let mut attr_name = String::new();
    let mut attr_val = String::new();
    for attr in e.attributes().flatten() {
        let k = String::from_utf8_lossy(attr.key.as_ref());
        let v = String::from_utf8_lossy(&attr.value);
        if k == "name" {
            attr_name = v.to_ascii_lowercase();
        } else if k == "value" {
            attr_val = v.to_string();
        }
    }
    match attr_name.as_str() {
        "seeders" => *seeders = attr_val.parse().unwrap_or(*seeders),
        "peers" | "leechers" => *leechers = attr_val.parse().unwrap_or(*leechers),
        "infohash" => *info_hash = Some(attr_val),
        "magneturl" => *magnet_url = Some(attr_val),
        "size" if *size_bytes == 0 => *size_bytes = attr_val.parse().unwrap_or(0),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_anime_release;

    #[test]
    fn test_parse_torznab_rss_sample() -> Result<()> {
        let sample_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
          <channel>
            <item>
              <title>[SubsPlease] Dandadan - 07 (1080p) [1234ABCD].mkv</title>
              <guid>https://nyaa.si/view/999999</guid>
              <link>https://nyaa.si/download/999999.torrent</link>
              <size>1450000000</size>
              <torznab:attr name="seeders" value="150" />
              <torznab:attr name="infohash" value="1234567890abcdef1234567890abcdef12345678" />
            </item>
          </channel>
        </rss>"#;

        let items = parse_torznab_xml(sample_xml, 2)?;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seeders, 150);
        assert_eq!(items[0].size_bytes, 1_450_000_000);
        assert_eq!(
            items[0].info_hash.as_deref(),
            Some("1234567890abcdef1234567890abcdef12345678")
        );

        let mut cfg = AppConfig::default();
        cfg.filters.allowed_groups = vec!["SubsPlease".to_string()];
        let client = ProwlarrClient::new(cfg)?;
        let parsed = parse_anime_release(&items[0].title);
        assert!(client.check_filters(&items[0], &parsed).is_ok());

        // Test search query construction for catch-up
        let queries = ProwlarrClient::build_missing_episodes_queries(&parsed);
        assert_eq!(queries[0], "Dandadan SubsPlease 1080p");
        assert_eq!(queries[1], "Dandadan SubsPlease");
        assert_eq!(queries[2], "Dandadan");
        assert_eq!(queries[3], "Dandadan SubsPlease batch");

        Ok(())
    }
}
