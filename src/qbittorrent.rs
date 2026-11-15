use crate::config::AppConfig;
use crate::models::DiscoveredRelease;
use anyhow::{Context, Result};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{debug, info};

#[derive(Debug, Clone, Deserialize)]
pub struct QbitTorrentInfo {
    pub hash: String,
    pub name: String,
    pub state: String,
    pub progress: f64,
    pub save_path: String,
    #[serde(default)]
    pub content_path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: String,
}

impl QbitTorrentInfo {
    pub fn is_completed(&self) -> bool {
        if self.progress >= 0.9999 {
            return true;
        }
        matches!(
            self.state.as_str(),
            "uploading" | "stalledUP" | "pausedUP" | "StoppedUP" | "queuedUP" | "forcedUP"
        )
    }

    pub fn is_errored(&self) -> bool {
        matches!(self.state.as_str(), "error" | "missingFiles")
    }
}

#[derive(Clone)]
pub struct QbittorrentClient {
    http: Client,
    config: AppConfig,
}

impl QbittorrentClient {
    pub fn new(config: AppConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to create qBittorrent HTTP client")?;

        Ok(Self { http, config })
    }

    fn base_url(&self) -> String {
        self.config
            .qbittorrent
            .url
            .trim_end_matches('/')
            .to_string()
    }

    /// Returns the configured qBittorrent API key (`Authorization: Bearer <api_key>`),
    /// or an error if it is missing or still set to a template placeholder.
    fn api_key(&self) -> Result<&str> {
        let key = self.config.qbittorrent.api_key.trim();
        if key.is_empty() || key.starts_with("REPLACE_") || key.starts_with("your_") {
            anyhow::bail!(
                "qBittorrent API key is not configured (set ZBRR_QBITTORRENT_API_KEY in .env)"
            );
        }
        Ok(key)
    }

    fn qbit_get(&self, url: &str) -> Result<reqwest::RequestBuilder> {
        let key = self.api_key()?;
        Ok(self.http.get(url).bearer_auth(key))
    }

    fn qbit_post(&self, url: &str) -> Result<reqwest::RequestBuilder> {
        let key = self.api_key()?;
        Ok(self.http.post(url).bearer_auth(key))
    }

    /// Tests connectivity using Bearer API key authentication and returns qBittorrent application version.
    pub async fn test_connection(&self) -> Result<String> {
        let url = format!("{}/api/v2/app/version", self.base_url());
        let resp = self
            .qbit_get(&url)?
            .send()
            .await
            .with_context(|| format!("Failed to connect to qBittorrent at {url}"))?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "qBittorrent API key authentication or version check failed with HTTP {}",
                resp.status()
            );
        }
        let version = resp.text().await?.trim().to_string();
        Ok(format!("qBittorrent {version}"))
    }

    async fn ensure_category(&self, category: &str) -> Result<()> {
        if category.trim().is_empty() {
            return Ok(());
        }
        let url = format!("{}/api/v2/torrents/createCategory", self.base_url());
        let _ = self
            .qbit_post(&url)?
            .form(&[("category", category), ("savePath", "")])
            .send()
            .await;
        Ok(())
    }

    fn attach_prowlarr_api_key_if_needed(&self, url: &str) -> String {
        let api_key = self.config.prowlarr.api_key.trim();
        if api_key.is_empty() || url.contains("apikey=") {
            return url.to_string();
        }

        let is_prowlarr = url.starts_with(&self.config.prowlarr.url)
            || url.contains("/api/v1/indexer/")
            || url.contains("/1/api?");

        if is_prowlarr {
            if url.contains('?') {
                format!("{url}&apikey={api_key}")
            } else {
                format!("{url}?apikey={api_key}")
            }
        } else {
            url.to_string()
        }
    }

    /// Adds a discovered release to qBittorrent under `staging_category` and `tag`,
    /// returning the torrent's 40-char hex `info_hash`.
    pub async fn add_source_download(&self, release: &DiscoveredRelease) -> Result<String> {
        self.ensure_category(&self.config.qbittorrent.staging_category)
            .await?;

        let add_url = format!("{}/api/v2/torrents/add", self.base_url());
        let mut expected_hash = release
            .info_hash
            .as_ref()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty());

        if expected_hash.is_none() {
            if let Some(mag) = &release.magnet_url {
                expected_hash = extract_btih_from_magnet(mag);
            }
        }

        // Prefer downloading the .torrent file bytes if download_url is HTTP(S) so we can compute exact infohash
        let mut torrent_bytes_opt: Option<Vec<u8>> = None;
        if let Some(dl_url) = &release.download_url {
            if dl_url.starts_with("http") {
                let auth_dl_url = self.attach_prowlarr_api_key_if_needed(dl_url);
                let mut req = self.http.get(&auth_dl_url);
                if !self.config.prowlarr.api_key.trim().is_empty() {
                    req = req.header("X-Api-Key", self.config.prowlarr.api_key.trim());
                }
                if let Ok(resp) = req.send().await {
                    if resp.status().is_success() {
                        if let Ok(bytes) = resp.bytes().await {
                            if bytes.first().copied() == Some(b'd') {
                                if expected_hash.is_none() {
                                    expected_hash = compute_infohash_from_torrent_bytes(&bytes);
                                }
                                torrent_bytes_opt = Some(bytes.to_vec());
                            }
                        }
                    }
                }
            }
        }

        let mut form = Form::new()
            .text("category", self.config.qbittorrent.staging_category.clone())
            .text("tags", self.config.qbittorrent.tag.clone());

        if !self.config.qbittorrent.save_path.trim().is_empty() {
            form = form.text("savepath", self.config.qbittorrent.save_path.clone());
        }

        if let Some(bytes) = torrent_bytes_opt {
            let part = Part::bytes(bytes)
                .file_name("source.torrent")
                .mime_str("application/x-bittorrent")?;
            form = form.part("torrents", part);
        } else if let Some(mag) = &release.magnet_url {
            form = form.text("urls", mag.clone());
        } else if let Some(dl_url) = &release.download_url {
            let auth_url = self.attach_prowlarr_api_key_if_needed(dl_url);
            form = form.text("urls", auth_url);
        } else {
            anyhow::bail!("Release has no usable download_url or magnet_url");
        }

        let resp = self
            .qbit_post(&add_url)?
            .multipart(form)
            .send()
            .await
            .context("Failed to POST /api/v2/torrents/add to qBittorrent")?;

        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent add torrent returned HTTP {}", resp.status());
        }

        // Verify that the torrent was actually registered in qBittorrent!
        let wait_limit = Duration::from_secs(15);
        let poll_step = Duration::from_millis(500);
        let start = Instant::now();

        while start.elapsed() < wait_limit {
            if let Some(ref hash) = expected_hash {
                if let Ok(Some(info)) = self.get_torrent_info(hash).await {
                    info!(
                        title = %release.title,
                        hash = %info.hash,
                        category = %self.config.qbittorrent.staging_category,
                        "Added source torrent to qBittorrent and verified registration"
                    );
                    return Ok(info.hash.to_ascii_lowercase());
                }
            }

            // Check staging category for title or hash match
            if let Ok(torrents) = self
                .list_torrents(Some(&self.config.qbittorrent.staging_category))
                .await
            {
                for t in torrents {
                    if t.name.eq_ignore_ascii_case(&release.title)
                        || release.title.contains(&t.name)
                        || t.name.contains(&release.title)
                    {
                        info!(
                            title = %release.title,
                            hash = %t.hash,
                            "Resolved and verified info_hash from qBittorrent staging category"
                        );
                        return Ok(t.hash.to_ascii_lowercase());
                    }
                }
            }

            tokio::time::sleep(poll_step).await;
        }

        // Final check for expected_hash in any category
        if let Some(hash) = expected_hash {
            if let Ok(Some(info)) = self.get_torrent_info(&hash).await {
                return Ok(info.hash.to_ascii_lowercase());
            }
        }

        anyhow::bail!(
            "Added torrent '{}' to qBittorrent but it failed to appear in client after 15s (invalid URL/magnet or connection error)",
            release.title
        )
    }

    pub async fn get_torrent_info(&self, info_hash: &str) -> Result<Option<QbitTorrentInfo>> {
        let url = format!("{}/api/v2/torrents/info", self.base_url());
        let resp = self
            .qbit_get(&url)?
            .query(&[("hashes", info_hash.trim().to_ascii_lowercase())])
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent torrents/info returned HTTP {}", resp.status());
        }

        let mut list: Vec<QbitTorrentInfo> = resp.json().await?;
        Ok(list.pop())
    }

    pub async fn list_torrents(&self, category: Option<&str>) -> Result<Vec<QbitTorrentInfo>> {
        let url = format!("{}/api/v2/torrents/info", self.base_url());
        let mut req = self.qbit_get(&url)?;
        if let Some(cat) = category {
            req = req.query(&[("category", cat)]);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent torrents/info returned HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    /// Polls `/api/v2/torrents/info` until download reaches 100% completion.
    pub async fn wait_for_completion(&self, info_hash: &str) -> Result<QbitTorrentInfo> {
        let timeout = Duration::from_secs(self.config.qbittorrent.download_timeout_secs.max(60));
        let poll_interval = Duration::from_secs(self.config.qbittorrent.poll_interval_secs.max(2));
        let started = Instant::now();
        let not_found_timeout = Duration::from_secs(30);
        let mut not_found_since: Option<Instant> = None;

        loop {
            if let Some(info) = self.get_torrent_info(info_hash).await? {
                not_found_since = None;

                if info.is_completed() {
                    info!(
                        name = %info.name,
                        hash = %info.hash,
                        save_path = %info.save_path,
                        content_path = %info.content_path,
                        "Download completed in qBittorrent (100%)"
                    );
                    return Ok(info);
                }

                if info.is_errored() {
                    anyhow::bail!(
                        "Torrent '{}' ({}) entered error state '{}' in qBittorrent",
                        info.name,
                        info.hash,
                        info.state
                    );
                }

                debug!(
                    name = %info.name,
                    progress_pct = format!("{:.1}%", info.progress * 100.0),
                    state = %info.state,
                    "Waiting for qBittorrent download completion"
                );
            } else {
                let first_not_found = not_found_since.get_or_insert_with(Instant::now);
                let elapsed_not_found = first_not_found.elapsed();

                debug!(
                    hash = info_hash,
                    elapsed_secs = elapsed_not_found.as_secs(),
                    "Torrent not yet visible in qBittorrent info list"
                );

                if elapsed_not_found >= not_found_timeout {
                    anyhow::bail!(
                        "Torrent with hash '{}' is not present in qBittorrent (not found for {}s)",
                        info_hash,
                        not_found_timeout.as_secs()
                    );
                }
            }

            if started.elapsed() >= timeout {
                anyhow::bail!(
                    "Timed out after {}s waiting for torrent {} to finish downloading",
                    timeout.as_secs(),
                    info_hash
                );
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Injects the re-signed `animez.to` `.torrent` file into qBittorrent pointing to the exact
    /// same `save_path` with `skip_checking=true` and `category = animez-seeding`.
    /// Preserves the original source torrent untouched!
    pub async fn inject_seeding_torrent(
        &self,
        torrent_file_path: &Path,
        save_path: &str,
    ) -> Result<()> {
        self.ensure_category(&self.config.qbittorrent.seeding_category)
            .await?;

        let bytes = fs::read(torrent_file_path).with_context(|| {
            format!(
                "Failed to read tracker torrent for auto-reseed: {}",
                torrent_file_path.display()
            )
        })?;
        let filename = torrent_file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("animez-seed.torrent")
            .to_string();

        let part = Part::bytes(bytes)
            .file_name(filename)
            .mime_str("application/x-bittorrent")?;

        let form = Form::new()
            .part("torrents", part)
            .text("savepath", save_path.to_string())
            .text("category", self.config.qbittorrent.seeding_category.clone())
            .text("tags", self.config.qbittorrent.tag.clone())
            .text("skip_checking", "true")
            .text("paused", "false");

        let add_url = format!("{}/api/v2/torrents/add", self.base_url());
        let resp = self
            .qbit_post(&add_url)?
            .multipart(form)
            .send()
            .await
            .context("Failed to inject re-signed torrent into qBittorrent")?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "qBittorrent failed to add seeding torrent: HTTP {}",
                resp.status()
            );
        }

        info!(
            torrent = %torrent_file_path.display(),
            save_path = save_path,
            category = %self.config.qbittorrent.seeding_category,
            "Injected tracker torrent into qBittorrent with skip_checking=true for immediate seeding"
        );
        Ok(())
    }

    /// Deletes a torrent from qBittorrent without deleting the downloaded media files on disk.
    pub async fn delete_torrent(&self, info_hash: &str) -> Result<()> {
        let url = format!("{}/api/v2/torrents/delete", self.base_url());
        let resp = self
            .qbit_post(&url)?
            .form(&[
                ("hashes", info_hash.trim().to_ascii_lowercase()),
                ("deleteFiles", "false".to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("Failed to delete torrent {info_hash} from qBittorrent"))?;

        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent delete torrent returned HTTP {}", resp.status());
        }

        info!(
            hash = %info_hash,
            "Deleted original source torrent from qBittorrent (kept downloaded media files on disk)"
        );
        Ok(())
    }
}

/// Extracts a 40-char hex infohash from a `magnet:?xt=urn:btih:...` URI.
pub fn extract_btih_from_magnet(magnet: &str) -> Option<String> {
    let lower = magnet.to_ascii_lowercase();
    let idx = lower.find("urn:btih:")?;
    let rest = &lower[idx + "urn:btih:".len()..];
    let hash_part = rest.split('&').next()?.trim();
    if hash_part.len() == 40 && hash_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(hash_part.to_string());
    }
    None
}

/// Extracts the raw `4:info` dictionary slice from a `.torrent` byte buffer and hashes it with SHA-1.
pub fn compute_infohash_from_torrent_bytes(bytes: &[u8]) -> Option<String> {
    let val: serde_bencode::value::Value = serde_bencode::from_bytes(bytes).ok()?;
    if let serde_bencode::value::Value::Dict(mut map) = val {
        let info_val = map.remove(b"info".as_slice())?;
        let info_bencoded = serde_bencode::to_bytes(&info_val).ok()?;
        let mut hasher = Sha1::new();
        hasher.update(&info_bencoded);
        return Some(hex::encode(hasher.finalize()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_btih_from_magnet() {
        let mag = "magnet:?xt=urn:btih:1234567890ABCDEF1234567890ABCDEF12345678&dn=Test";
        assert_eq!(
            extract_btih_from_magnet(mag).as_deref(),
            Some("1234567890abcdef1234567890abcdef12345678")
        );
    }

    #[test]
    fn test_attach_prowlarr_api_key_if_needed() {
        let mut config = AppConfig::default();
        config.prowlarr.url = "http://prowlarr.lan:9696".to_string();
        config.prowlarr.api_key = "secret_prowlarr_key".to_string();

        let client = QbittorrentClient::new(config).unwrap();

        // Already has apikey
        let u1 = "http://prowlarr.lan:9696/api/v1/indexer/8/download?link=abc&apikey=existing";
        assert_eq!(client.attach_prowlarr_api_key_if_needed(u1), u1);

        // Prowlarr URL with query params
        let u2 = "http://prowlarr.lan:9696/api/v1/indexer/8/download?link=abc";
        assert_eq!(
            client.attach_prowlarr_api_key_if_needed(u2),
            "http://prowlarr.lan:9696/api/v1/indexer/8/download?link=abc&apikey=secret_prowlarr_key"
        );

        // Non-prowlarr URL
        let u3 = "https://nyaa.si/download/12345.torrent";
        assert_eq!(client.attach_prowlarr_api_key_if_needed(u3), u3);
    }
}
