use crate::config::AppConfig;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCookieJar {
    pub user_agent: String,
    pub updated_at: DateTime<Utc>,
    pub cookies: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct FlareSolverrPage {
    pub url: String,
    pub status: u16,
    pub html: String,
    pub user_agent: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlareSolverrRequest<'a> {
    cmd: &'a str,
    url: &'a str,
    max_timeout: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cookies: Vec<FlareSolverrCookieInput>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FlareSolverrCookieInput {
    name: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlareSolverrResponse {
    status: String,
    message: Option<String>,
    solution: Option<FlareSolverrSolution>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlareSolverrSolution {
    url: String,
    status: u16,
    response: String,
    user_agent: String,
    #[serde(default)]
    cookies: Vec<FlareSolverrCookieOutput>,
}

#[derive(Debug, Deserialize)]
struct FlareSolverrCookieOutput {
    name: String,
    value: String,
}

#[derive(Clone)]
pub struct FlareSolverrClient {
    http: Client,
    config: AppConfig,
    jar: Arc<RwLock<CachedCookieJar>>,
}

impl FlareSolverrClient {
    pub fn new(config: AppConfig) -> Result<Self> {
        let timeout_secs = (config.flaresolverr.max_timeout_ms / 1000).max(30) + 15;
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("Failed to create FlareSolverr HTTP client")?;

        let initial_jar = Self::load_or_init_jar(&config);

        Ok(Self {
            http,
            config,
            jar: Arc::new(RwLock::new(initial_jar)),
        })
    }

    fn load_or_init_jar(config: &AppConfig) -> CachedCookieJar {
        let mut cookies = BTreeMap::new();
        let mut user_agent = config.animez.user_agent.clone();

        if config.flaresolverr.cookies_file.exists() {
            if let Ok(raw) = fs::read_to_string(&config.flaresolverr.cookies_file) {
                if let Ok(saved) = serde_json::from_str::<CachedCookieJar>(&raw) {
                    cookies = saved.cookies;
                    if !saved.user_agent.trim().is_empty() {
                        user_agent = saved.user_agent;
                    }
                }
            }
        }

        // Apply any explicit cookies configured via .env if missing or overridden
        if !config.animez.session_cookie.trim().is_empty() {
            cookies.insert(
                "animez_session".to_string(),
                config.animez.session_cookie.trim().to_string(),
            );
            cookies.insert(
                "animez-session".to_string(),
                config.animez.session_cookie.trim().to_string(),
            );
        }
        if !config.animez.remember_cookie.trim().is_empty() {
            // Support either "name=value" or raw value for remember_web_*
            if let Some((k, v)) = config.animez.remember_cookie.split_once('=') {
                cookies.insert(k.trim().to_string(), v.trim().to_string());
            } else {
                cookies.insert(
                    "remember_web_59ba36addc2b2f9401580f014c7f58ea4e30989d".to_string(),
                    config.animez.remember_cookie.trim().to_string(),
                );
            }
        }
        if !config.animez.cf_clearance.trim().is_empty() {
            cookies.insert(
                "cf_clearance".to_string(),
                config.animez.cf_clearance.trim().to_string(),
            );
        }

        CachedCookieJar {
            user_agent,
            updated_at: Utc::now(),
            cookies,
        }
    }

    pub async fn persist_jar(&self) -> Result<()> {
        let snapshot = self.jar.read().await.clone();
        if let Some(parent) = self.config.flaresolverr.cookies_file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let serialized = serde_json::to_string_pretty(&snapshot)?;
        fs::write(&self.config.flaresolverr.cookies_file, serialized).with_context(|| {
            format!(
                "Failed to write cookies cache to {}",
                self.config.flaresolverr.cookies_file.display()
            )
        })?;
        Ok(())
    }

    pub async fn user_agent(&self) -> String {
        self.jar.read().await.user_agent.clone()
    }

    pub async fn cookie_header(&self) -> String {
        let jar = self.jar.read().await;
        jar.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub async fn has_session_cookies(&self) -> bool {
        let jar = self.jar.read().await;
        !jar.cookies.is_empty()
    }

    /// Updates stored cookies from `Set-Cookie` headers received during direct `reqwest` calls.
    pub async fn ingest_set_cookie_headers(&self, headers: &reqwest::header::HeaderMap) {
        let mut changed = false;
        {
            let mut jar = self.jar.write().await;
            for val in headers.get_all(reqwest::header::SET_COOKIE) {
                if let Ok(s) = val.to_str() {
                    if let Some(first_pair) = s.split(';').next() {
                        if let Some((name, value)) = first_pair.split_once('=') {
                            let name = name.trim().to_string();
                            let value = value.trim().to_string();
                            if !name.is_empty() && !value.is_empty() {
                                jar.cookies.insert(name, value);
                                jar.updated_at = Utc::now();
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        if changed {
            let _ = self.persist_jar().await;
        }
    }

    /// Tests connectivity to the FlareSolverr server (`http://localhost:8191/v1` or root `/`).
    pub async fn test_connection(&self) -> Result<String> {
        let base = self
            .config
            .flaresolverr
            .url
            .trim_end_matches("/v1")
            .trim_end_matches('/');
        let resp = self
            .http
            .get(base)
            .send()
            .await
            .with_context(|| format!("Failed to connect to FlareSolverr at {base}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                return Ok(format!("FlareSolverr {version} (HTTP {status})"));
            }
        }
        Ok(format!("FlareSolverr reachable (HTTP {status})"))
    }

    /// Solves a Cloudflare challenge or fetches a page via FlareSolverr (`request.get`),
    /// caching all returned cookies and the exact `User-Agent` to `~/.zbrr/cookies.json`.
    pub async fn solve_get(&self, target_url: &str) -> Result<FlareSolverrPage> {
        self.solve_get_opts(target_url, true).await
    }

    /// Fetches a page via FlareSolverr without forwarding existing session cookies,
    /// essential for starting a clean authentication session for Cloudflare Turnstile.
    pub async fn solve_get_without_session(&self, target_url: &str) -> Result<FlareSolverrPage> {
        self.solve_get_opts(target_url, false).await
    }

    async fn solve_get_opts(
        &self,
        target_url: &str,
        include_session: bool,
    ) -> Result<FlareSolverrPage> {
        let domain = url::Url::parse(target_url)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()));

        let input_cookies: Vec<FlareSolverrCookieInput> = {
            let jar = self.jar.read().await;
            jar.cookies
                .iter()
                .filter(|(k, _)| {
                    if include_session {
                        true
                    } else {
                        // Strip session cookies when requesting login page so a fresh CSRF + Turnstile session starts
                        !k.contains("session") && !k.contains("remember")
                    }
                })
                .map(|(k, v)| FlareSolverrCookieInput {
                    name: k.clone(),
                    value: v.clone(),
                    domain: domain.clone(),
                })
                .collect()
        };

        let payload = FlareSolverrRequest {
            cmd: "request.get",
            url: target_url,
            max_timeout: self.config.flaresolverr.max_timeout_ms.max(30000),
            cookies: input_cookies,
        };

        info!(
            url = target_url,
            "Requesting Cloudflare clearance via FlareSolverr"
        );
        let resp = self
            .http
            .post(&self.config.flaresolverr.url)
            .json(&payload)
            .send()
            .await
            .with_context(|| {
                format!(
                    "Failed to POST to FlareSolverr at {}",
                    self.config.flaresolverr.url
                )
            })?;

        if !resp.status().is_success() {
            anyhow::bail!("FlareSolverr HTTP error: {}", resp.status());
        }

        let body: FlareSolverrResponse = resp
            .json()
            .await
            .context("Failed to deserialize FlareSolverr JSON response")?;

        if body.status != "ok" {
            anyhow::bail!(
                "FlareSolverr error status '{}': {}",
                body.status,
                body.message.unwrap_or_default()
            );
        }

        let sol = body
            .solution
            .context("FlareSolverr response missing 'solution' object")?;

        {
            let mut jar = self.jar.write().await;
            if !sol.user_agent.trim().is_empty() {
                jar.user_agent = sol.user_agent.clone();
            }
            for c in &sol.cookies {
                jar.cookies.insert(c.name.clone(), c.value.clone());
            }
            jar.updated_at = Utc::now();
            debug!(
                cookie_count = jar.cookies.len(),
                user_agent = %jar.user_agent,
                "Updated cached cookies and User-Agent from FlareSolverr"
            );
        }

        self.persist_jar().await?;

        Ok(FlareSolverrPage {
            url: sol.url,
            status: sol.status,
            html: sol.response,
            user_agent: sol.user_agent,
        })
    }
}
