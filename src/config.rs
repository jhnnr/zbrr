use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub prowlarr: ProwlarrConfig,
    pub filters: FilterConfig,
    pub qbittorrent: QbittorrentConfig,
    pub packager: PackagerConfig,
    pub flaresolverr: FlareSolverrConfig,
    pub animez: AnimezConfig,
    pub jikan: JikanConfig,
}

/// Resolves the current user's home directory across Windows and Unix.
pub fn resolve_home_dir() -> PathBuf {
    if let Ok(v) = env::var("USERPROFILE") {
        if !v.trim().is_empty() {
            return PathBuf::from(v.trim());
        }
    }
    if let Ok(v) = env::var("HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v.trim());
        }
    }
    if let (Ok(drive), Ok(path)) = (env::var("HOMEDRIVE"), env::var("HOMEPATH")) {
        let combined = format!("{drive}{path}");
        if !combined.trim().is_empty() {
            return PathBuf::from(combined.trim());
        }
    }
    PathBuf::from(".")
}

/// Returns the default zbrr user data directory: `~/.zbrr`.
pub fn default_zbrr_dir() -> PathBuf {
    resolve_home_dir().join(".zbrr")
}

/// Expands a leading `~` or `~/` / `~\` into the user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        resolve_home_dir()
    } else if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        resolve_home_dir().join(rest)
    } else if let Ok(stripped) = path.strip_prefix("~") {
        resolve_home_dir().join(stripped)
    } else {
        path.to_path_buf()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub data_dir: PathBuf,
    pub poll_interval_secs: u64,
    pub max_per_cycle: usize,
    pub dry_run: bool,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            data_dir: default_zbrr_dir(),
            poll_interval_secs: 300,
            max_per_cycle: 5,
            dry_run: false,
            min_delay_ms: 2000,
            max_delay_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProwlarrConfig {
    pub url: String,
    pub api_key: String,
    /// "api" (/api/v1/search) or "torznab" (/{id}/api?t=rss)
    pub mode: String,
    /// Numeric Prowlarr indexer IDs to poll (empty = all enabled indexers unless `indexers` is set)
    pub indexer_ids: Vec<i64>,
    /// Indexer names to poll (case-insensitive name/substring match via `/api/v1/indexer`, e.g. ["SubsPlease", "Nyaa"])
    pub indexers: Vec<String>,
    /// Indexer names to exclude from polling (e.g. ["AnimeZ"])
    pub exclude_indexers: Vec<String>,
    /// Optional Prowlarr indexer ID configured for animez.to (used for fast API dupe checking)
    pub animez_indexer_id: i64,
    pub categories: Vec<i32>,
    pub query: String,
    pub timeout_secs: u64,
}

impl Default for ProwlarrConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:9696".to_string(),
            api_key: String::new(),
            mode: "api".to_string(),
            indexer_ids: Vec::new(),
            indexers: Vec::new(),
            exclude_indexers: vec!["AnimeZ".to_string()],
            animez_indexer_id: 0,
            categories: vec![5070],
            query: String::new(),
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub include_regex: Vec<String>,
    pub exclude_regex: Vec<String>,
    pub allowed_groups: Vec<String>,
    pub allowed_resolutions: Vec<String>,
    pub allowed_sources: Vec<String>,
    pub allowed_codecs: Vec<String>,
    pub min_seeders: u32,
    pub min_size_bytes: u64,
    pub max_size_bytes: u64,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            include_regex: vec!["(?i)\\b(1080p|2160p|720p)\\b".to_string()],
            exclude_regex: vec!["(?i)\\b(CAM|TS|R5|360p|480p)\\b".to_string()],
            allowed_groups: Vec::new(),
            allowed_resolutions: vec!["1080p".to_string(), "2160p".to_string()],
            allowed_sources: Vec::new(),
            allowed_codecs: Vec::new(),
            min_seeders: 1,
            min_size_bytes: 50 * 1024 * 1024,
            max_size_bytes: 100 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QbittorrentConfig {
    pub url: String,
    pub api_key: String,
    pub staging_category: String,
    pub seeding_category: String,
    pub tag: String,
    pub save_path: String,
    pub download_timeout_secs: u64,
    pub poll_interval_secs: u64,
    pub remote_path_prefix: String,
    pub local_path_prefix: String,
}

impl Default for QbittorrentConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8080".to_string(),
            api_key: String::new(),
            staging_category: "animez-staging".to_string(),
            seeding_category: "animez-seeding".to_string(),
            tag: "animez-auto".to_string(),
            save_path: String::new(),
            download_timeout_secs: 14400,
            poll_interval_secs: 10,
            remote_path_prefix: String::new(),
            local_path_prefix: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PackagerConfig {
    pub ffmpeg_bin: String,
    pub ffprobe_bin: String,
    pub mediainfo_bin: String,
    pub screenshot_count: usize,
    pub screenshot_positions: Vec<f64>,
    pub screenshot_format: String,
    pub screenshot_quality: u32,
}

impl Default for PackagerConfig {
    fn default() -> Self {
        Self {
            ffmpeg_bin: "ffmpeg".to_string(),
            ffprobe_bin: "ffprobe".to_string(),
            mediainfo_bin: "mediainfo".to_string(),
            screenshot_count: 6,
            screenshot_positions: vec![0.10, 0.25, 0.40, 0.55, 0.70, 0.85],
            screenshot_format: "jpg".to_string(),
            screenshot_quality: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FlareSolverrConfig {
    pub url: String,
    pub max_timeout_ms: u64,
    pub cookies_file: PathBuf,
}

impl Default for FlareSolverrConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8191/v1".to_string(),
            max_timeout_ms: 60000,
            cookies_file: default_zbrr_dir().join("cookies.json"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimezConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
    #[serde(alias = "passkey")]
    pub pid: String,
    pub announce_url: String,
    pub login_path: String,
    pub upload_path: String,
    pub search_path: String,
    pub session_cookie: String,
    pub remember_cookie: String,
    pub cf_clearance: String,
    pub user_agent: String,
    pub anonymous: bool,
    pub image_upload_mode: String,
    pub enforce_ongoing_catchup: bool,
}

impl Default for AnimezConfig {
    fn default() -> Self {
        Self {
            base_url: "https://animez.to".to_string(),
            username: String::new(),
            password: String::new(),
            pid: String::new(),
            announce_url: "https://tracker.animez.to/announce".to_string(),
            login_path: "/login".to_string(),
            upload_path: "/upload".to_string(),
            search_path: "/torrents".to_string(),
            session_cookie: String::new(),
            remember_cookie: String::new(),
            cf_clearance: String::new(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".to_string(),
            anonymous: false,
            image_upload_mode: "direct".to_string(),
            enforce_ongoing_catchup: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JikanConfig {
    pub base_url: String,
    pub enabled: bool,
    pub cache_ttl_days: u32,
}

impl Default for JikanConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.jikan.moe/v4".to_string(),
            enabled: true,
            cache_ttl_days: 30,
        }
    }
}

/// Loads environment variables from a file, falling back to line-by-line parsing
/// if dotenvy encounters syntax errors (e.g. unquoted Windows backslash paths).
pub fn load_env_file_safe(path: &Path) {
    if let Err(e) = dotenvy::from_path(path) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "dotenvy encountered a syntax issue; using resilient key=value line parser"
        );
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once('=') {
                    let key = k.trim();
                    let mut val = v.trim();
                    if ((val.starts_with('\'') && val.ends_with('\''))
                        || (val.starts_with('"') && val.ends_with('"')))
                        && val.len() >= 2
                    {
                        val = &val[1..val.len() - 1];
                    }
                    if !key.is_empty() {
                        env::set_var(key, val);
                    }
                }
            }
        }
    }
}

impl AppConfig {
    /// Load configuration strictly from `.env` (or `~/.zbrr/.env`, or explicit path) and system environment variables.
    pub fn load(env_file: Option<&Path>) -> Result<Self> {
        let zbrr_dir = default_zbrr_dir();

        if let Some(explicit_path) = env_file {
            let expanded = expand_tilde(explicit_path);
            if expanded.exists() {
                load_env_file_safe(&expanded);
            } else {
                anyhow::bail!(
                    "Specified environment file does not exist: {}",
                    expanded.display()
                );
            }
        } else {
            let local_env = Path::new(".env");
            if local_env.exists() {
                load_env_file_safe(local_env);
            } else {
                let home_env = zbrr_dir.join(".env");
                if home_env.exists() {
                    load_env_file_safe(&home_env);
                } else {
                    let _ = dotenvy::dotenv();
                }
            }
        }

        let mut cfg = AppConfig::default();
        cfg.apply_env_overrides();

        cfg.general.data_dir = expand_tilde(&cfg.general.data_dir);
        cfg.flaresolverr.cookies_file = expand_tilde(&cfg.flaresolverr.cookies_file);
        // If cookies_file was left as the legacy "data/cookies.json", redirect it to the data_dir
        if cfg.flaresolverr.cookies_file == Path::new("data/cookies.json") {
            cfg.flaresolverr.cookies_file = cfg.general.data_dir.join("cookies.json");
        }

        cfg.ensure_directories()?;
        cfg.migrate_legacy_data();
        Ok(cfg)
    }

    /// Automatically migrates legacy local `./data/state.db` and `./data/cookies.json`
    /// to the user data directory (`~/.zbrr`) if not already present.
    pub fn migrate_legacy_data(&self) {
        let legacy_db = PathBuf::from("data/state.db");
        let target_db = self.db_path();
        if legacy_db.exists() && !target_db.exists() {
            if let Some(parent) = target_db.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if fs::copy(&legacy_db, &target_db).is_ok() {
                tracing::info!(
                    from = %legacy_db.display(),
                    to = %target_db.display(),
                    "Migrated existing state.db to ~/.zbrr"
                );
            }
        }

        let legacy_cookies = PathBuf::from("data/cookies.json");
        let target_cookies = &self.flaresolverr.cookies_file;
        if legacy_cookies.exists() && !target_cookies.exists() {
            if let Some(parent) = target_cookies.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if fs::copy(&legacy_cookies, target_cookies).is_ok() {
                tracing::info!(
                    from = %legacy_cookies.display(),
                    to = %target_cookies.display(),
                    "Migrated existing cookies.json to ~/.zbrr"
                );
            }
        }
    }

    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = env::var("ZBRR_DATA_DIR") {
            if !v.trim().is_empty() {
                self.general.data_dir = expand_tilde(&PathBuf::from(v.trim()));
                if self.flaresolverr.cookies_file.ends_with("cookies.json") {
                    self.flaresolverr.cookies_file = self.general.data_dir.join("cookies.json");
                }
            }
        }
        if let Ok(v) = env::var("ZBRR_FLARESOLVERR_COOKIES_FILE") {
            if !v.trim().is_empty() {
                self.flaresolverr.cookies_file = expand_tilde(&PathBuf::from(v.trim()));
            }
        }
        if let Ok(v) = env::var("ZBRR_POLL_INTERVAL_SECS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.general.poll_interval_secs = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_MAX_PER_CYCLE") {
            if let Ok(n) = v.trim().parse::<usize>() {
                self.general.max_per_cycle = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_DRY_RUN") {
            if let Ok(b) = v.trim().parse::<bool>() {
                self.general.dry_run = b;
            }
        }
        if let Ok(v) = env::var("ZBRR_MIN_DELAY_MS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.general.min_delay_ms = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_MAX_DELAY_MS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.general.max_delay_ms = n;
            }
        }

        if let Ok(v) = env::var("ZBRR_PROWLARR_URL") {
            if !v.is_empty() {
                self.prowlarr.url = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_API_KEY") {
            if !v.is_empty() {
                self.prowlarr.api_key = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_MODE") {
            if !v.is_empty() {
                self.prowlarr.mode = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_INDEXERS") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.prowlarr.indexers = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_INDEXER_IDS") {
            let list: Vec<i64> = v
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            if !list.is_empty() {
                self.prowlarr.indexer_ids = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_EXCLUDE_INDEXERS") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.prowlarr.exclude_indexers = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_ANIMEZ_INDEXER_ID") {
            if let Ok(id) = v.trim().parse::<i64>() {
                self.prowlarr.animez_indexer_id = id;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_CATEGORIES") {
            let list: Vec<i32> = v
                .split(',')
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .collect();
            if !list.is_empty() {
                self.prowlarr.categories = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_QUERY") {
            self.prowlarr.query = v;
        }
        if let Ok(v) = env::var("ZBRR_PROWLARR_TIMEOUT_SECS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.prowlarr.timeout_secs = n;
            }
        }

        // Filter environment overrides
        if let Ok(v) = env::var("ZBRR_FILTERS_INCLUDE_REGEX") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.filters.include_regex = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_EXCLUDE_REGEX") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.filters.exclude_regex = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_ALLOWED_GROUPS") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.filters.allowed_groups = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_ALLOWED_RESOLUTIONS") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.filters.allowed_resolutions = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_ALLOWED_SOURCES") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.filters.allowed_sources = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_ALLOWED_CODECS") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.filters.allowed_codecs = list;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_MIN_SEEDERS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.filters.min_seeders = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_MIN_SIZE_BYTES") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.filters.min_size_bytes = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_FILTERS_MAX_SIZE_BYTES") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.filters.max_size_bytes = n;
            }
        }

        // qBittorrent environment overrides
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_URL") {
            if !v.is_empty() {
                self.qbittorrent.url = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_API_KEY") {
            if !v.is_empty() {
                self.qbittorrent.api_key = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_STAGING_CATEGORY") {
            if !v.is_empty() {
                self.qbittorrent.staging_category = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_SEEDING_CATEGORY") {
            if !v.is_empty() {
                self.qbittorrent.seeding_category = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_TAG") {
            if !v.is_empty() {
                self.qbittorrent.tag = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_SAVE_PATH") {
            self.qbittorrent.save_path = v;
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_DOWNLOAD_TIMEOUT_SECS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.qbittorrent.download_timeout_secs = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_POLL_INTERVAL_SECS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.qbittorrent.poll_interval_secs = n;
            }
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_REMOTE_PATH_PREFIX") {
            self.qbittorrent.remote_path_prefix = v;
        }
        if let Ok(v) = env::var("ZBRR_QBITTORRENT_LOCAL_PATH_PREFIX") {
            self.qbittorrent.local_path_prefix = v;
        }

        // Packager environment overrides
        if let Ok(v) = env::var("ZBRR_PACKAGER_FFMPEG_BIN") {
            if !v.is_empty() {
                self.packager.ffmpeg_bin = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_PACKAGER_FFPROBE_BIN") {
            if !v.is_empty() {
                self.packager.ffprobe_bin = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_PACKAGER_MEDIAINFO_BIN") {
            if !v.is_empty() {
                self.packager.mediainfo_bin = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_PACKAGER_SCREENSHOT_COUNT") {
            if let Ok(n) = v.trim().parse::<usize>() {
                self.packager.screenshot_count = n;
            }
        }
        // AnimeZ upload rules (Section 6) strictly require a minimum of 6 screenshots for anime uploads
        if self.packager.screenshot_count < 6 {
            self.packager.screenshot_count = 6;
        }
        if let Ok(v) = env::var("ZBRR_PACKAGER_SCREENSHOT_POSITIONS") {
            let list: Vec<f64> = v
                .split(',')
                .filter_map(|s| s.trim().parse::<f64>().ok())
                .collect();
            if !list.is_empty() {
                self.packager.screenshot_positions = list;
            }
        }
        // Ensure positions list matches at least screenshot_count
        if self.packager.screenshot_positions.len() < self.packager.screenshot_count {
            self.packager.screenshot_positions = vec![0.10, 0.25, 0.40, 0.55, 0.70, 0.85];
        }
        if let Ok(v) = env::var("ZBRR_PACKAGER_SCREENSHOT_FORMAT") {
            let fmt = v.trim().to_lowercase();
            if !fmt.is_empty() {
                self.packager.screenshot_format = fmt;
            }
        }
        if let Ok(v) = env::var("ZBRR_PACKAGER_SCREENSHOT_QUALITY") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.packager.screenshot_quality = n.clamp(1, 31);
            }
        }

        // FlareSolverr environment overrides
        if let Ok(v) = env::var("ZBRR_FLARESOLVERR_URL") {
            if !v.is_empty() {
                self.flaresolverr.url = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_FLARESOLVERR_MAX_TIMEOUT_MS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.flaresolverr.max_timeout_ms = n;
            }
        }

        // AnimeZ environment overrides
        if let Ok(v) = env::var("ZBRR_ANIMEZ_BASE_URL") {
            if !v.is_empty() {
                self.animez.base_url = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_USERNAME") {
            if !v.is_empty() {
                self.animez.username = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_PASSWORD") {
            if !v.is_empty() {
                self.animez.password = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_PID").or_else(|_| env::var("ZBRR_ANIMEZ_PASSKEY")) {
            if !v.is_empty() {
                self.animez.pid = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_ANNOUNCE_URL") {
            if !v.is_empty() {
                self.animez.announce_url = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_LOGIN_PATH") {
            if !v.is_empty() {
                self.animez.login_path = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_UPLOAD_PATH") {
            if !v.is_empty() {
                self.animez.upload_path = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_SEARCH_PATH") {
            if !v.is_empty() {
                self.animez.search_path = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_ANONYMOUS") {
            if let Ok(b) = v.trim().parse::<bool>() {
                self.animez.anonymous = b;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_IMAGE_UPLOAD_MODE") {
            if !v.is_empty() {
                self.animez.image_upload_mode = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_ENFORCE_ONGOING_CATCHUP") {
            if let Ok(b) = v.trim().parse::<bool>() {
                self.animez.enforce_ongoing_catchup = b;
            }
        }
        // Auto-build announce_url from `pid` if announce_url is still a placeholder or default
        let pid_trimmed = self.animez.pid.trim();
        if !pid_trimmed.is_empty()
            && !pid_trimmed.starts_with("REPLACE_")
            && !pid_trimmed.starts_with("your_")
            && (self.animez.announce_url.contains("REPLACE_")
                || self.animez.announce_url.contains("your_")
                || self.animez.announce_url == "https://tracker.animez.to/announce")
        {
            self.animez.announce_url = format!("https://tracker.animez.to/{pid_trimmed}/announce");
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_SESSION_COOKIE") {
            if !v.is_empty() {
                self.animez.session_cookie = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_REMEMBER_COOKIE") {
            if !v.is_empty() {
                self.animez.remember_cookie = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_CF_CLEARANCE") {
            if !v.is_empty() {
                self.animez.cf_clearance = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_ANIMEZ_USER_AGENT") {
            if !v.is_empty() {
                self.animez.user_agent = v;
            }
        }

        // Jikan environment overrides
        if let Ok(v) = env::var("ZBRR_JIKAN_BASE_URL") {
            if !v.is_empty() {
                self.jikan.base_url = v;
            }
        }
        if let Ok(v) = env::var("ZBRR_JIKAN_ENABLED") {
            if let Ok(b) = v.trim().parse::<bool>() {
                self.jikan.enabled = b;
            }
        }
        if let Ok(v) = env::var("ZBRR_JIKAN_CACHE_TTL_DAYS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.jikan.cache_ttl_days = n;
            }
        }
    }

    pub fn ensure_directories(&self) -> Result<()> {
        fs::create_dir_all(&self.general.data_dir).with_context(|| {
            format!(
                "Failed to create data directory: {}",
                self.general.data_dir.display()
            )
        })?;
        fs::create_dir_all(self.staging_dir()).with_context(|| {
            format!(
                "Failed to create staging directory: {}",
                self.staging_dir().display()
            )
        })?;
        if let Some(parent) = self.flaresolverr.cookies_file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.general.data_dir.join("state.db")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.general.data_dir.join("staging")
    }

    /// Maps a remote qBittorrent path (e.g., `/downloads/Anime/Show.mkv` in Docker)
    /// to a local host path (e.g., `/media/downloads/Anime/Show.mkv` or `D:\Downloads\Anime\Show.mkv`) if configured.
    pub fn map_qbit_path_to_local(&self, qbit_path: &str) -> PathBuf {
        let remote_prefix = self.qbittorrent.remote_path_prefix.trim();
        let local_prefix = self.qbittorrent.local_path_prefix.trim();

        if !remote_prefix.is_empty() && !local_prefix.is_empty() {
            if let Some(stripped) = qbit_path.strip_prefix(remote_prefix) {
                let trimmed = stripped.trim_start_matches(['/', '\\']);
                let normalized = trimmed.replace('/', std::path::MAIN_SEPARATOR_STR);
                return PathBuf::from(local_prefix).join(normalized);
            }
        }
        PathBuf::from(qbit_path)
    }

    /// Returns a polite random delay duration between `min_delay_ms` and `max_delay_ms`.
    pub fn polite_delay_duration(&self) -> Duration {
        let min = self.general.min_delay_ms;
        let max = self.general.max_delay_ms.max(min);
        if min == max {
            return Duration::from_millis(min);
        }
        let millis = rand::thread_rng().gen_range(min..=max);
        Duration::from_millis(millis)
    }

    /// Sleeps for a polite random jitter interval (2-5s by default) to respect rate limits.
    pub async fn polite_sleep(&self) {
        let dur = self.polite_delay_duration();
        if !dur.is_zero() {
            tokio::time::sleep(dur).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_local_env() {
        if Path::new(".env").exists() {
            let res = dotenvy::from_path(".env");
            println!("dotenvy result: {:?}", res);
            assert!(res.is_ok(), "Failed to load .env: {:?}", res);
        }
    }

    #[test]
    fn test_load_env_file_safe_with_unquoted_backslash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let env_file = temp_dir.path().join(".env.test");
        fs::write(
            &env_file,
            "TEST_VAR_REMOTE=/downloads\nTEST_VAR_LOCAL=\\\\server\\share\\folder\nTEST_VAR_NEXT=http://docker.lan:8191/v1\n",
        ).unwrap();

        load_env_file_safe(&env_file);
        assert_eq!(env::var("TEST_VAR_REMOTE").unwrap(), "/downloads");
        assert_eq!(env::var("TEST_VAR_LOCAL").unwrap(), "\\\\server\\share\\folder");
        assert_eq!(env::var("TEST_VAR_NEXT").unwrap(), "http://docker.lan:8191/v1");
    }
}
