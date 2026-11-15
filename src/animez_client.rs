use crate::config::AppConfig;
use crate::flaresolverr::FlareSolverrClient;
use crate::models::{
    FormElementSchema, FormFieldSchema, FormInspectionReport, MalMetadata, PackageBundle,
    ParsedRelease, SelectOptionSchema, UploadResult,
};
use anyhow::{Context, Result};
use chrono::{Datelike, Utc};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, COOKIE, REFERER, USER_AGENT};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use scraper::{Html, Selector};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct TrackerTorrentEntry {
    pub title: String,
    pub url: String,
    pub resolution: Option<String>,
    pub group: Option<String>,
    pub video_codec: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnimeSearchResponse {
    #[serde(default)]
    animes: Vec<AnimeSearchItem>,
    #[serde(default)]
    torrents: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnimeSearchItem {
    id: u64,
    title: String,
    #[serde(default)]
    title_romaji: Option<String>,
    #[serde(default)]
    title_native: Option<String>,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    external_ids: Option<AnimeSearchExternalIds>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnimeSearchExternalIds {
    #[serde(default)]
    mal: Option<serde_json::Value>,
    #[serde(default)]
    anidb: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct AnimezClient {
    http: Client,
    config: AppConfig,
    flaresolverr: FlareSolverrClient,
}

impl AnimezClient {
    pub fn new(config: AppConfig, flaresolverr: FlareSolverrClient) -> Result<Self> {
        let http = Client::builder()
            .gzip(true)
            .timeout(Duration::from_secs(45))
            .build()
            .context("Failed to create AnimeZ HTTP client")?;

        Ok(Self {
            http,
            config,
            flaresolverr,
        })
    }

    async fn build_headers(&self, referer: Option<&str>) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let ua = self.flaresolverr.user_agent().await;
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&ua).unwrap_or_else(|_| {
                HeaderValue::from_static(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
                )
            }),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );

        let cookie_str = self.flaresolverr.cookie_header().await;
        if !cookie_str.is_empty() {
            if let Ok(val) = HeaderValue::from_str(&cookie_str) {
                headers.insert(COOKIE, val);
            }
        }

        if let Some(ref_url) = referer {
            if let Ok(val) = HeaderValue::from_str(ref_url) {
                headers.insert(REFERER, val);
            }
        }

        Ok(headers)
    }

    async fn build_ajax_headers(&self, referer: Option<&str>) -> Result<HeaderMap> {
        let mut headers = self.build_headers(referer).await?;
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/plain, */*"),
        );
        headers.insert(
            HeaderName::from_static("x-requested-with"),
            HeaderValue::from_static("XMLHttpRequest"),
        );
        Ok(headers)
    }

    async fn build_screenshot_headers(
        &self,
        csrf_token: &str,
        referer: Option<&str>,
    ) -> Result<HeaderMap> {
        let mut headers = self.build_headers(referer).await?;
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static("x-requested-with"),
            HeaderValue::from_static("XMLHttpRequest"),
        );
        if let Ok(token_val) = HeaderValue::from_str(csrf_token) {
            headers.insert(HeaderName::from_static("x-csrf-token"), token_val);
        }
        Ok(headers)
    }

    pub fn has_credentials(&self) -> bool {
        let u = self.config.animez.username.trim();
        let p = self.config.animez.password.trim();
        !u.is_empty()
            && !p.is_empty()
            && !u.starts_with("REPLACE_")
            && !u.starts_with("your_")
            && !p.starts_with("REPLACE_")
            && !p.starts_with("your_")
    }

    /// Performs an authenticated GET request to `animez.to`, falling back to FlareSolverr
    /// when cookies are empty or Cloudflare returns a 403/503 challenge, and automatically
    /// logging in with `username` + `password` if redirected to the login page.
    pub async fn get_html(&self, url: &str) -> Result<String> {
        let html = self.fetch_html_raw(url).await?;

        if !url.contains("login") && Self::is_login_page(&html) && self.has_credentials() {
            info!(
                url = url,
                username = %self.config.animez.username,
                "Session is not authenticated; logging in to animez.to with configured username/password"
            );
            self.login_with_credentials().await?;
            return self.fetch_html_raw(url).await;
        }

        Ok(html)
    }

    async fn fetch_html_raw(&self, url: &str) -> Result<String> {
        self.config.polite_sleep().await;

        if !self.flaresolverr.has_session_cookies().await {
            debug!(url = url, "No cached cookies present; initializing session via FlareSolverr");
            let solved = self.flaresolverr.solve_get(url).await?;
            return Ok(solved.html);
        }

        let headers = self.build_headers(None).await?;
        let resp = self.http.get(url).headers(headers).send().await;

        match resp {
            Ok(r) => {
                self.flaresolverr.ingest_set_cookie_headers(r.headers()).await;
                let status = r.status();
                let body = r.text().await.unwrap_or_default();

                if Self::is_cloudflare_challenge(status, &body) {
                    warn!(
                        url = url,
                        status = %status,
                        "Cloudflare challenge detected; refreshing clearance via FlareSolverr"
                    );
                    let solved = self.flaresolverr.solve_get(url).await?;
                    return Ok(solved.html);
                }

                if !status.is_success() {
                    anyhow::bail!("GET {url} returned HTTP {status}");
                }

                Ok(body)
            }
            Err(e) => {
                warn!(error = %e, url = url, "Direct GET failed; attempting FlareSolverr fallback");
                let solved = self.flaresolverr.solve_get(url).await?;
                Ok(solved.html)
            }
        }
    }

    pub fn is_login_page(html: &str) -> bool {
        let doc = Html::parse_document(html);
        let pass_sel = Selector::parse(r#"input[name="password"][type="password"]"#).unwrap();
        let logout_sel = Selector::parse(r#"a[href*="logout"], form[action*="logout"]"#).unwrap();
        doc.select(&pass_sel).next().is_some() && doc.select(&logout_sel).next().is_none()
    }

    /// Authenticates against `animez.to/login` (AvistaZ Laravel auth flow) using
    /// configured `username` and `password`, persisting session cookies to `data/cookies.json`.
    pub async fn login_with_credentials(&self) -> Result<()> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let login_url = format!(
            "{base}/{}",
            self.config.animez.login_path.trim_start_matches('/')
        );

        // Because animez.to/login requires solving Cloudflare Turnstile client-side JavaScript,
        // we fetch the login page via FlareSolverr without stale session cookies, retrying
        // until the Turnstile challenge has executed and populated `cf-turnstile-response`.
        let mut login_html = String::new();
        let mut csrf_opt = None;
        let mut turnstile_opt = None;

        for attempt in 1..=4 {
            info!(
                attempt = attempt,
                url = %login_url,
                "Requesting login page via FlareSolverr to solve Cloudflare Turnstile"
            );
            let solved = self.flaresolverr.solve_get_without_session(&login_url).await?;
            csrf_opt = Self::extract_csrf_token(&solved.html);
            turnstile_opt = Self::extract_turnstile_token(&solved.html);
            login_html = solved.html;

            if turnstile_opt.is_some() && csrf_opt.is_some() {
                info!(
                    attempt = attempt,
                    "Cloudflare Turnstile challenge successfully solved"
                );
                break;
            }

            if attempt < 4 {
                warn!(
                    attempt = attempt,
                    "Turnstile token not yet ready in FlareSolverr solution; retrying in 2s..."
                );
                tokio::time::sleep(Duration::from_millis(2000)).await;
            }
        }

        let csrf_token = csrf_opt
            .context("Could not find Laravel CSRF `_token` on animez.to login page")?;

        let post_url = Self::extract_login_action_url(base, &login_url, &login_html);
        let user_field_name = Self::extract_username_field_name(&login_html);

        self.config.polite_sleep().await;
        let headers = self.build_headers(Some(&login_url)).await?;

        let username = self.config.animez.username.trim().to_string();
        let password = self.config.animez.password.clone();

        let mut params = vec![
            ("_token".to_string(), csrf_token),
            (user_field_name.clone(), username.clone()),
            ("password".to_string(), password),
            ("remember".to_string(), "1".to_string()),
        ];
        if user_field_name != "username" {
            params.push(("username".to_string(), username.clone()));
        }
        if let Some(turnstile) = turnstile_opt {
            info!("Attaching Cloudflare Turnstile verification token to login payload");
            params.push(("cf-turnstile-response".to_string(), turnstile));
        } else {
            warn!("No cf-turnstile-response found on login page; login may fail if Turnstile is enforced");
        }

        // Explicitly use a client with redirect::Policy::none() so that Set-Cookie headers
        // on the initial 302 Found response (containing animez-session and remember_web_*)
        // are captured directly rather than discarded during automatic redirect following.
        let no_redirect_http = Client::builder()
            .gzip(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(45))
            .build()
            .context("Failed to build no-redirect HTTP client for login")?;

        let resp = no_redirect_http
            .post(&post_url)
            .headers(headers)
            .form(&params)
            .send()
            .await
            .with_context(|| format!("Failed to POST login credentials to {post_url}"))?;

        self.flaresolverr
            .ingest_set_cookie_headers(resp.headers())
            .await;

        let status = resp.status();
        let redirect_target = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if status.is_redirection() {
            if redirect_target.contains("/login") {
                anyhow::bail!(
                    "animez.to login rejected credentials: redirected back to '{redirect_target}'"
                );
            }
            info!(
                redirect_target = redirect_target,
                "Successfully authenticated to animez.to and captured session cookies"
            );
        } else if status.is_success() {
            info!("Submitted login credentials (HTTP {status}) and updated cookies cache");
        } else {
            anyhow::bail!("animez.to login POST returned HTTP {status}");
        }

        Ok(())
    }

    fn extract_login_action_url(base_url: &str, default_login_url: &str, html: &str) -> String {
        let doc = Html::parse_document(html);
        let form_sel = Selector::parse("form").unwrap();
        let pass_sel = Selector::parse(r#"input[name="password"]"#).unwrap();
        for form in doc.select(&form_sel) {
            if form.select(&pass_sel).next().is_some() {
                if let Some(action) = form.value().attr("action") {
                    if !action.trim().is_empty() {
                        if action.starts_with("http") {
                            return action.to_string();
                        }
                        return format!(
                            "{}/{}",
                            base_url.trim_end_matches('/'),
                            action.trim_start_matches('/')
                        );
                    }
                }
            }
        }
        default_login_url.to_string()
    }

    fn extract_upload_action_url(base_url: &str, default_upload_url: &str, html: &str) -> String {
        let doc = Html::parse_document(html);
        let form_sel = Selector::parse("form").unwrap();
        let torrent_sel = Selector::parse(r#"input[type="file"][name="torrent"], input[name="torrent"]"#).unwrap();
        for form in doc.select(&form_sel) {
            if form.select(&torrent_sel).next().is_some() {
                if let Some(action) = form.value().attr("action") {
                    if !action.trim().is_empty() {
                        if action.starts_with("http") {
                            return action.to_string();
                        }
                        return format!(
                            "{}/{}",
                            base_url.trim_end_matches('/'),
                            action.trim_start_matches('/')
                        );
                    }
                }
            }
        }
        default_upload_url.to_string()
    }

    fn detect_release_format(parsed: &ParsedRelease) -> String {
        let upper = parsed.raw_title.to_ascii_uppercase();
        if upper.contains("MOVIE") || upper.contains("GEKIJOUBAN") {
            "MOVIE".to_string()
        } else if upper.contains("OVA") {
            "OVA".to_string()
        } else if upper.contains("ONA") {
            "ONA".to_string()
        } else if upper.contains("SPECIAL") || upper.contains(" SP ") || upper.ends_with(" SP") {
            "SPECIAL".to_string()
        } else {
            "TV".to_string()
        }
    }

    fn detect_resolution(parsed: &ParsedRelease) -> String {
        if let Some(res) = &parsed.resolution {
            let r = res.to_ascii_lowercase();
            if r.contains("2160") || r.contains("4k") {
                "2160p".to_string()
            } else if r.contains("1080") {
                "1080p".to_string()
            } else if r.contains("720") {
                "720p".to_string()
            } else if r.contains("576") {
                "576p".to_string()
            } else if r.contains("480") {
                "480p".to_string()
            } else {
                "1080p".to_string()
            }
        } else {
            "1080p".to_string()
        }
    }

    fn detect_rip_type(parsed: &ParsedRelease) -> String {
        let upper = parsed.raw_title.to_ascii_uppercase();
        if upper.contains("BLURAY RAW") || upper.contains("BD RAW") {
            "BluRay Raw".to_string()
        } else if upper.contains("REMUX") && (upper.contains("BLURAY") || upper.contains("BD")) {
            "BluRay Remux".to_string()
        } else if upper.contains("BDRIP") {
            "BDRip".to_string()
        } else if upper.contains("BRRIP") {
            "BRRip".to_string()
        } else if upper.contains("BLURAY") || upper.contains("BLU-RAY") || upper.contains(" BD") {
            "BluRay".to_string()
        } else if upper.contains("WEB-DL") || upper.contains("WEBDL") {
            "WEB-DL".to_string()
        } else if upper.contains("WEBRIP")
            || upper.contains("WEB")
            || upper.contains("CR ")
            || upper.contains("NF ")
        {
            "WEBRip".to_string()
        } else if upper.contains("HDTV") {
            "HDTV".to_string()
        } else if upper.contains("DVDRIP") {
            "DVDRip".to_string()
        } else if upper.contains("DVD") {
            "DVD".to_string()
        } else if let Some(src) = &parsed.source {
            match src.to_ascii_uppercase().as_str() {
                "WEBRIP" => "WEBRip".to_string(),
                "WEB-DL" | "WEBDL" => "WEB-DL".to_string(),
                "BLURAY" | "BD" => "BluRay".to_string(),
                "HDTV" => "HDTV".to_string(),
                "DVD" => "DVD".to_string(),
                _ => "WEBRip".to_string(),
            }
        } else {
            "WEBRip".to_string()
        }
    }

    fn detect_collection_type(
        parsed: &ParsedRelease,
    ) -> (&'static str, Option<String>, Option<String>) {
        if parsed.is_season_pack {
            ("complete", None, None)
        } else if let Some(ep) = parsed.episode {
            ("single", Some(ep.to_string()), None)
        } else {
            ("complete", None, None)
        }
    }

    fn normalize_title_for_lookup(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn extract_form_errors(html: &str) -> String {
        let doc = Html::parse_document(html);
        if let Ok(sel) = Selector::parse(
            ".alert-danger, .invalid-feedback, .text-danger, ul.errors li, .alert, div[role='alert']",
        ) {
            let mut msgs = Vec::new();
            for el in doc.select(&sel) {
                let t = el.text().collect::<String>().trim().to_string();
                if !t.is_empty() && !msgs.contains(&t) && !t.eq_ignore_ascii_case("×") {
                    msgs.push(t);
                }
            }
            if !msgs.is_empty() {
                return msgs.join("; ");
            }
        }
        "Tracker rejected upload and redirected back to upload page".to_string()
    }

    fn extract_username_field_name(html: &str) -> String {
        let doc = Html::parse_document(html);
        for candidate in ["email_or_username", "username", "email", "login"] {
            let sel_str = format!(r#"input[name="{candidate}"]"#);
            if let Ok(sel) = Selector::parse(&sel_str) {
                if doc.select(&sel).next().is_some() {
                    return candidate.to_string();
                }
            };
        }
        "username".to_string()
    }

    pub fn is_cloudflare_challenge(status: StatusCode, body: &str) -> bool {
        if matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE
        ) {
            return true;
        }
        let lower = body.to_ascii_lowercase();
        lower.contains("cf-browser-verification")
            || lower.contains("challenge-platform")
            || (lower.contains("<title>just a moment") && lower.contains("cloudflare"))
    }

    /// Tests connectivity and login state on `animez.to`.
    pub async fn test_connection(&self) -> Result<String> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let html = self.get_html(base).await?;
        let doc = Html::parse_document(&html);
        let title_sel = Selector::parse("title").unwrap();
        let page_title = doc
            .select(&title_sel)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_else(|| "Unknown Title".to_string());

        let is_logged_in = html.contains("/logout")
            || html.contains("upload")
            || html.contains("passkey");

        Ok(format!(
            "Reachable (title='{}', authenticated={})",
            page_title, is_logged_in
        ))
    }

    /// Extracts the Laravel CSRF token (`input[name="_token"]` or `meta[name="csrf-token"]`).
    pub fn extract_csrf_token(html: &str) -> Option<String> {
        let doc = Html::parse_document(html);

        if let Ok(input_sel) = Selector::parse(r#"input[name="_token"]"#) {
            if let Some(el) = doc.select(&input_sel).next() {
                if let Some(val) = el.value().attr("value") {
                    if !val.trim().is_empty() {
                        return Some(val.trim().to_string());
                    }
                }
            }
        }

        if let Ok(meta_sel) = Selector::parse(r#"meta[name="csrf-token"]"#) {
            if let Some(el) = doc.select(&meta_sel).next() {
                if let Some(val) = el.value().attr("content") {
                    if !val.trim().is_empty() {
                        return Some(val.trim().to_string());
                    }
                }
            }
        }

        None
    }

    /// Extracts the Cloudflare Turnstile verification response token (`input[name="cf-turnstile-response"]`)
    /// if solved by FlareSolverr or present on the page.
    pub fn extract_turnstile_token(html: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        if let Ok(sel) = Selector::parse(r#"input[name="cf-turnstile-response"]"#) {
            for el in doc.select(&sel) {
                if let Some(val) = el.value().attr("value") {
                    if !val.trim().is_empty() {
                        return Some(val.trim().to_string());
                    }
                }
            }
        }
        None
    }

    /// Implements `zbrr inspect-form`: fetches `https://animez.to/upload` (or reads local HTML)
    /// and dumps all `<form>`, `<input>`, `<select>`, and `<textarea>` names/options to JSON.
    pub async fn inspect_upload_form(
        &self,
        from_html_file: Option<&Path>,
    ) -> Result<FormInspectionReport> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let upload_url = format!(
            "{base}/{}",
            self.config.animez.upload_path.trim_start_matches('/')
        );

        let html = if let Some(path) = from_html_file {
            fs::read_to_string(path)
                .with_context(|| format!("Failed to read HTML file: {}", path.display()))?
        } else {
            self.get_html(&upload_url).await?
        };

        Ok(Self::parse_upload_form_html(&upload_url, &html))
    }

    pub fn parse_upload_form_html(url: &str, html: &str) -> FormInspectionReport {
        let doc = Html::parse_document(html);
        let title_sel = Selector::parse("title").unwrap();
        let form_sel = Selector::parse("form").unwrap();
        let field_sel = Selector::parse("input, select, textarea").unwrap();
        let option_sel = Selector::parse("option").unwrap();

        let page_title = doc
            .select(&title_sel)
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let csrf_token_present = Self::extract_csrf_token(html).is_some();
        let mut forms = Vec::new();

        for form_el in doc.select(&form_sel) {
            let mut fields = Vec::new();
            for field_el in form_el.select(&field_sel) {
                let tag = field_el.value().name().to_string();
                let field_type = field_el.value().attr("type").map(|s| s.to_string());
                let name = field_el.value().attr("name").map(|s| s.to_string());
                let id = field_el.value().attr("id").map(|s| s.to_string());
                let required = field_el.value().attr("required").is_some();
                let placeholder = field_el.value().attr("placeholder").map(|s| s.to_string());
                let value = field_el.value().attr("value").map(|s| s.to_string());

                let mut options = Vec::new();
                if tag == "select" {
                    for opt_el in field_el.select(&option_sel) {
                        let opt_val = opt_el
                            .value()
                            .attr("value")
                            .unwrap_or_default()
                            .to_string();
                        let label = opt_el.text().collect::<String>().trim().to_string();
                        let selected = opt_el.value().attr("selected").is_some();
                        options.push(SelectOptionSchema {
                            value: opt_val,
                            label,
                            selected,
                        });
                    }
                }

                fields.push(FormFieldSchema {
                    tag,
                    field_type,
                    name,
                    id,
                    required,
                    placeholder,
                    value,
                    options,
                });
            }

            forms.push(FormElementSchema {
                id: form_el.value().attr("id").map(|s| s.to_string()),
                name: form_el.value().attr("name").map(|s| s.to_string()),
                action: form_el.value().attr("action").map(|s| s.to_string()),
                method: form_el.value().attr("method").map(|s| s.to_string()),
                enctype: form_el.value().attr("enctype").map(|s| s.to_string()),
                fields,
            });
        }

        FormInspectionReport {
            url: url.to_string(),
            page_title,
            inspected_at: Utc::now(),
            csrf_token_present,
            forms,
        }
    }

    /// Searches `animez.to/torrents` for matching releases.
    pub async fn search_torrents(
        &self,
        search_query: &str,
        mal_id: Option<u64>,
    ) -> Result<Vec<TrackerTorrentEntry>> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let search_path = self.config.animez.search_path.trim_start_matches('/');
        let mut url = url::Url::parse(&format!("{base}/{search_path}"))?;
        {
            let mut qp = url.query_pairs_mut();
            if !search_query.trim().is_empty() {
                // animez.to expects query parameter `q`
                qp.append_pair("q", search_query.trim());
            }
            if let Some(mid) = mal_id {
                qp.append_pair("mal_id", &mid.to_string());
            }
        }

        let html = self.get_html(url.as_str()).await?;
        Ok(Self::parse_torrent_search_html(base, &html))
    }

    fn extract_year_from_title(title: &str) -> Option<u32> {
        static Y_RE: OnceLock<Regex> = OnceLock::new();
        let y_re = Y_RE.get_or_init(|| Regex::new(r"\((?:19|20)\d{2}\)").unwrap());
        static DIGITS_RE: OnceLock<Regex> = OnceLock::new();
        let digits_re = DIGITS_RE.get_or_init(|| Regex::new(r"\d{4}").unwrap());
        y_re.find(title)
            .and_then(|m| digits_re.find(m.as_str()))
            .and_then(|m| m.as_str().parse::<u32>().ok())
    }

    fn extract_item_season(item: &AnimeSearchItem) -> Option<u32> {
        static S_RE: OnceLock<Regex> = OnceLock::new();
        let s_re = S_RE.get_or_init(|| {
            Regex::new(r"(?i)\b(?:(\d{1,2})(?:st|nd|rd|th)\s*Season|(?:First|Second|Third|Fourth|Fifth|Sixth)\s*Season|Season\s*(\d{1,2})|\bII\b|\bIII\b|\bIV\b|\bVI\b)\b").unwrap()
        });

        for name in [&item.title, item.title_romaji.as_deref().unwrap_or("")] {
            if let Some(caps) = s_re.captures(name) {
                if let Some(n) = caps.get(1) {
                    return n.as_str().parse::<u32>().ok();
                } else if let Some(n) = caps.get(2) {
                    return n.as_str().parse::<u32>().ok();
                } else {
                    let lower = name.to_ascii_lowercase();
                    if lower.contains("second") || lower.contains("2nd") || lower.contains(" ii") {
                        return Some(2);
                    } else if lower.contains("third") || lower.contains("3rd") || lower.contains(" iii") {
                        return Some(3);
                    } else if lower.contains("fourth") || lower.contains("4th") || lower.contains(" iv") {
                        return Some(4);
                    } else if lower.contains("fifth") || lower.contains("5th") {
                        return Some(5);
                    } else if lower.contains("sixth") || lower.contains("6th") || lower.contains(" vi") {
                        return Some(6);
                    } else if lower.contains("first") || lower.contains("1st") {
                        return Some(1);
                    }
                }
            }
        }
        None
    }

    fn score_anime_candidate(
        item: &AnimeSearchItem,
        target_title: &str,
        target_year: Option<u32>,
        target_season: Option<u32>,
        target_mal_id: Option<u64>,
    ) -> i32 {
        let titles_to_check = [
            Some(&item.title),
            item.title_romaji.as_ref(),
            item.title_native.as_ref(),
        ];

        let target_s = target_season.unwrap_or(1);
        let item_s = Self::extract_item_season(item);
        let item_title_year = titles_to_check
            .into_iter()
            .flatten()
            .find_map(|t| Self::extract_year_from_title(t));

        // 1. Direct MAL ID match (+1000, only if seasons are strictly compatible)
        if let Some(target_mal) = target_mal_id {
            if let Some(ext) = &item.external_ids {
                if let Some(mal_val) = &ext.mal {
                    let mal_str = match mal_val {
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::String(s) => s.trim().to_string(),
                        _ => String::new(),
                    };
                    if mal_str == target_mal.to_string() {
                        if let Some(is) = item_s {
                            if is != target_s {
                                return -999;
                            }
                        } else if target_s > 1 && item_title_year.is_none() {
                            return -999;
                        }
                        return 1000;
                    }
                }
            }
        }

        let norm_target = Self::normalize_title_for_lookup(target_title);
        static SEASON_STRIP_RE: OnceLock<Regex> = OnceLock::new();
        let season_strip_re = SEASON_STRIP_RE.get_or_init(|| {
            Regex::new(r"(?i)\b(?:\d{1,2}(?:st|nd|rd|th)|first|second|third|fourth|fifth|sixth|season)\s*(?:season|\d{1,2})?\b").unwrap()
        });
        let norm_target_base = season_strip_re
            .replace_all(&norm_target, " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let mut title_matched = false;
        let mut score = 0;

        static YEAR_STRIP_RE: OnceLock<Regex> = OnceLock::new();
        let year_strip_re = YEAR_STRIP_RE.get_or_init(|| {
            Regex::new(r"\b(?:19|20)\d{2}\b").unwrap()
        });

        for t_opt in titles_to_check.into_iter().flatten() {
            let norm_t = Self::normalize_title_for_lookup(t_opt);
            let norm_t_no_year = year_strip_re
                .replace_all(&norm_t, " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");

            if norm_t == norm_target
                || (!norm_target_base.is_empty() && norm_t == norm_target_base)
                || norm_t_no_year == norm_target
                || (!norm_target_base.is_empty() && norm_t_no_year == norm_target_base)
            {
                score += 100;
                title_matched = true;
                break;
            } else if (!norm_target_base.is_empty() && norm_target_base.len() >= 4 && norm_t.contains(&norm_target_base))
                || (norm_t.len() >= 4 && !norm_target_base.is_empty() && norm_target_base.contains(&norm_t))
                || (norm_target.len() >= 4 && norm_t.contains(&norm_target))
                || (!norm_target_base.is_empty() && norm_target_base.len() >= 4 && norm_t_no_year.contains(&norm_target_base))
            {
                score += 60;
                title_matched = true;
                break;
            }
        }

        if !title_matched {
            return -999;
        }

        if target_s > 1 {
            if let Some(is) = item_s {
                if is == target_s {
                    score += 150;
                } else {
                    return -999;
                }
            } else if item_title_year.is_some() {
                // Item has a year in title (e.g. "Yasei no Last Boss ga Arawareta! (2026)")
                // In AniDB/AnimeZ conventions, titles with (YYYY) represent sequels when the base title has no season suffix.
                score += 150;
            } else {
                // Item has NO season suffix and NO year in title: it is Season 1!
                return -999;
            }

            if let Some(st) = &item.status {
                if st.eq_ignore_ascii_case("releasing") {
                    score += 50;
                } else if st.eq_ignore_ascii_case("finished") {
                    score -= 40;
                }
            }

            let current_year = Utc::now().year();
            if let Some(y) = item.year {
                if y >= current_year - 1 {
                    score += 40;
                } else {
                    score -= 50;
                }
            }
        } else {
            // Target is Season 1
            if let Some(is) = item_s {
                if is > 1 {
                    return -999;
                } else {
                    score += 50;
                }
            } else if let Some(ity) = item_title_year {
                // Item has a year in title (e.g. (2026)), but target is Season 1
                if target_year == Some(ity) {
                    score += 100;
                } else {
                    return -999;
                }
            } else {
                // Clean base title with no season and no year is standard Season 1
                score += 100;
            }
        }

        if let (Some(y1), Some(y2)) = (target_year, item.year) {
            if (y1 as i32) == y2 {
                score += 50;
            } else {
                score -= 20;
            }
        }

        score
    }

    /// Resolves AnimeZ's internal anime entry (`(id, canonical_title)`) for a release using `https://animez.to/search?type=anime&q=...`.
    pub async fn lookup_anime_entry(
        &self,
        title: &str,
        year: Option<u32>,
        season: Option<u32>,
        mal_id: Option<u64>,
    ) -> Result<Option<(u64, String)>> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let mut url = url::Url::parse(&format!("{base}/search"))?;
        url.query_pairs_mut()
            .append_pair("type", "anime")
            .append_pair("q", title.trim());

        self.config.polite_sleep().await;
        let headers = self.build_ajax_headers(Some(url.as_str())).await?;
        let resp = self.http.get(url.as_str()).headers(headers).send().await?;
        if !resp.status().is_success() {
            warn!(
                status = %resp.status(),
                title = title,
                "Anime search endpoint returned non-success HTTP status"
            );
            return Ok(None);
        }

        let mut search_res: AnimeSearchResponse = resp.json().await.unwrap_or(AnimeSearchResponse {
            animes: Vec::new(),
            torrents: Vec::new(),
        });

        // If search returned empty and title contains season terms, try searching base title
        if search_res.animes.is_empty() {
            let stripped_query = title
                .split_whitespace()
                .take_while(|w| !w.eq_ignore_ascii_case("season") && !w.ends_with("nd") && !w.ends_with("rd") && !w.ends_with("th"))
                .collect::<Vec<_>>()
                .join(" ");
            if !stripped_query.is_empty() && stripped_query != title.trim() {
                let mut fallback_url = url::Url::parse(&format!("{base}/search"))?;
                fallback_url.query_pairs_mut()
                    .append_pair("type", "anime")
                    .append_pair("q", stripped_query.trim());
                if let Ok(f_resp) = self.http.get(fallback_url.as_str()).headers(self.build_ajax_headers(Some(fallback_url.as_str())).await?).send().await {
                    if f_resp.status().is_success() {
                        if let Ok(f_data) = f_resp.json::<AnimeSearchResponse>().await {
                            search_res = f_data;
                        }
                    }
                }
            }
        }

        if search_res.animes.is_empty() {
            debug!(title = title, "No anime entries returned in AnimeZ search");
            return Ok(None);
        }

        let mut scored_candidates: Vec<(i32, &AnimeSearchItem)> = search_res
            .animes
            .iter()
            .map(|item| {
                let sc = Self::score_anime_candidate(item, title, year, season, mal_id);
                (sc, item)
            })
            .collect();

        scored_candidates.sort_by_key(|b| std::cmp::Reverse(b.0));

        if let Some((score, best)) = scored_candidates.first() {
            if *score > 0 {
                // AniDB main title is required by AnimeZ naming guidelines
                let canonical_title = best
                    .title_romaji
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(&best.title);

                info!(
                    anime_id = best.id,
                    anime_title = %best.title,
                    canonical_title = canonical_title,
                    score = score,
                    season = ?season,
                    "Resolved best AnimeZ anime entry for release"
                );
                return Ok(Some((best.id, canonical_title.to_string())));
            }
        }

        // Fallback for Season 1 only if score >= 0
        if season.unwrap_or(1) == 1 {
            if let Some(first) = search_res.animes.first() {
                let canonical_title = first
                    .title_romaji
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(&first.title);
                info!(
                    anime_id = first.id,
                    anime_title = %first.title,
                    canonical_title = canonical_title,
                    "Resolved AnimeZ anime entry to first candidate as Season 1 fallback"
                );
                return Ok(Some((first.id, canonical_title.to_string())));
            }
        }

        Ok(None)
    }

    /// Resolves AnimeZ's internal anime ID (`anime_id`) for a release using `https://animez.to/search?type=anime&q=...`.
    pub async fn lookup_anime_id(
        &self,
        title: &str,
        year: Option<u32>,
        season: Option<u32>,
        mal_id: Option<u64>,
    ) -> Result<Option<u64>> {
        self.lookup_anime_entry(title, year, season, mal_id)
            .await
            .map(|opt| opt.map(|(id, _)| id))
    }

    /// Resolves AnimeZ's internal canonical anime title for a release using `https://animez.to/search?type=anime&q=...`.
    pub async fn lookup_anime_title(
        &self,
        title: &str,
        year: Option<u32>,
        season: Option<u32>,
        mal_id: Option<u64>,
    ) -> Result<Option<String>> {
        self.lookup_anime_entry(title, year, season, mal_id)
            .await
            .map(|opt| opt.map(|(_, t)| t))
    }

    /// Imports or resolves an anime entry on AnimeZ using `POST https://animez.to/anime` with an external URL (e.g. MAL or AniDB link).
    ///
    /// Returns `Ok(Some((anime_id, canonical_title)))` if successfully resolved/imported by AnimeZ.
    pub async fn import_or_resolve_anime(
        &self,
        external_url: &str,
        csrf_token: &str,
        referer: &str,
    ) -> Result<Option<(u64, String)>> {
        let clean_url = external_url.trim();
        if clean_url.is_empty() || !clean_url.starts_with("http") {
            return Ok(None);
        }

        self.config.polite_sleep().await;
        let base = self.config.animez.base_url.trim_end_matches('/');
        let url = format!("{base}/anime");

        let mut headers = self.build_ajax_headers(Some(referer)).await?;
        if let Ok(token_val) = HeaderValue::from_str(csrf_token) {
            headers.insert(HeaderName::from_static("x-csrf-token"), token_val);
        }
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let payload = serde_json::json!({
            "url": clean_url
        });

        debug!(url = clean_url, "Importing/resolving anime entry on AnimeZ via external URL");
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&payload)
            .send()
            .await?;

        self.flaresolverr
            .ingest_set_cookie_headers(resp.headers())
            .await;

        if !resp.status().is_success() {
            debug!(
                status = %resp.status(),
                url = clean_url,
                "AnimeZ /anime import endpoint returned non-success HTTP status"
            );
            return Ok(None);
        }

        #[derive(Debug, Deserialize)]
        struct AnimeImportResponse {
            anime: Option<AnimeImportItem>,
        }

        #[derive(Debug, Deserialize)]
        struct AnimeImportItem {
            id: u64,
            title: String,
        }

        let body: AnimeImportResponse = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                debug!(error = %e, "Failed parsing AnimeZ /anime import JSON response");
                return Ok(None);
            }
        };

        if let Some(item) = body.anime {
            info!(
                anime_id = item.id,
                title = %item.title,
                url = clean_url,
                "Successfully resolved/imported AnimeZ anime entry via external URL"
            );
            return Ok(Some((item.id, item.title)));
        }

        Ok(None)
    }

    /// Checks whether the user has any existing uploads for a given anime title and season on AnimeZ.
    pub async fn has_user_uploaded_prior_episode(
        &self,
        username: &str,
        anime_title: &str,
        season: Option<u32>,
    ) -> Result<bool> {
        let trimmed_user = username.trim();
        if trimmed_user.is_empty() || trimmed_user.starts_with("REPLACE_") {
            return Ok(false);
        }

        let base = self.config.animez.base_url.trim_end_matches('/');
        let mut url = url::Url::parse(&format!("{base}/profile/{trimmed_user}/uploads"))?;
        url.query_pairs_mut().append_pair("q", anime_title.trim());

        let html = match self.get_html(url.as_str()).await {
            Ok(h) => h,
            Err(e) => {
                debug!(error = %e, url = %url, "Could not fetch user profile uploads page");
                return Ok(false);
            }
        };

        let entries = Self::parse_torrent_search_html(base, &html);
        if entries.is_empty() {
            return Ok(false);
        }

        let target_s = season.unwrap_or(1);
        let norm_target = Self::normalize_title_for_lookup(anime_title);
        for entry in entries {
            let parsed_entry = crate::parser::parse_anime_release(&entry.title);
            let entry_s = parsed_entry.season.unwrap_or(1);
            if entry_s != target_s {
                continue;
            }
            let norm_entry = Self::normalize_title_for_lookup(&parsed_entry.clean_title);
            if norm_entry.contains(&norm_target) || norm_target.contains(&norm_entry) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn parse_torrent_search_html(base_url: &str, html: &str) -> Vec<TrackerTorrentEntry> {
        let doc = Html::parse_document(html);
        // Support AvistaZ / UNIT3D / custom Laravel torrent link patterns
        let link_sel = Selector::parse(
            r#"a.torrent-link, a.torrent-filename, a.view-torrent, td.torrent-name a, a[href*="/torrent/"], a[href*="/torrents/"]"#,
        )
        .unwrap();
        let tr_sel = Selector::parse("tr").unwrap();

        let mut results = Vec::new();
        let mut seen_urls = std::collections::HashSet::new();

        // 1. First attempt: iterate over table rows to capture resolution/codec badges in same row
        for tr in doc.select(&tr_sel) {
            if let Some(a) = tr.select(&link_sel).next() {
                let href = a.value().attr("href").unwrap_or_default();
                if href.contains("/download") || href.ends_with("/torrents") || href.contains("/edit") {
                    continue;
                }
                let title = a
                    .text()
                    .collect::<String>()
                    .trim()
                    .trim_start_matches('#')
                    .trim()
                    .to_string();
                if title.len() < 4 {
                    continue;
                }
                let full_url = if href.starts_with("http") {
                    href.to_string()
                } else {
                    format!(
                        "{}/{}",
                        base_url.trim_end_matches('/'),
                        href.trim_start_matches('/')
                    )
                };
                if !seen_urls.insert(full_url.clone()) {
                    continue;
                }

                let row_text = tr.text().collect::<String>();
                let mut parsed = crate::parser::parse_anime_release(&title);

                // If resolution was missing from title, inspect table row badges/text
                if parsed.resolution.is_none() {
                    let r_upper = row_text.to_ascii_uppercase();
                    if r_upper.contains("2160P") || r_upper.contains("4K") {
                        parsed.resolution = Some("2160p".to_string());
                    } else if r_upper.contains("1080P") {
                        parsed.resolution = Some("1080p".to_string());
                    } else if r_upper.contains("720P") {
                        parsed.resolution = Some("720p".to_string());
                    } else if r_upper.contains("576P") {
                        parsed.resolution = Some("576p".to_string());
                    } else if r_upper.contains("480P") {
                        parsed.resolution = Some("480p".to_string());
                    }
                }

                // If video codec was missing from title, inspect table row badges/text
                if parsed.video_codec.is_none() {
                    let r_upper = row_text.to_ascii_uppercase();
                    if r_upper.contains("HEVC") || r_upper.contains("X265") || r_upper.contains("H.265") || r_upper.contains("H265") {
                        parsed.video_codec = Some("HEVC".to_string());
                    } else if r_upper.contains("AVC") || r_upper.contains("X264") || r_upper.contains("H.264") || r_upper.contains("H264") {
                        parsed.video_codec = Some("H.264".to_string());
                    } else if r_upper.contains("AV1") {
                        parsed.video_codec = Some("AV1".to_string());
                    }
                }

                results.push(TrackerTorrentEntry {
                    title,
                    url: full_url,
                    resolution: parsed.resolution,
                    group: parsed.release_group,
                    video_codec: parsed.video_codec,
                });
            }
        }

        // 2. Fallback if no table rows were matched (e.g. non-table layout)
        if results.is_empty() {
            for a in doc.select(&link_sel) {
                let href = a.value().attr("href").unwrap_or_default();
                if href.contains("/download") || href.ends_with("/torrents") || href.contains("/edit") {
                    continue;
                }
                let title = a
                    .text()
                    .collect::<String>()
                    .trim()
                    .trim_start_matches('#')
                    .trim()
                    .to_string();
                if title.len() < 4 {
                    continue;
                }
                let full_url = if href.starts_with("http") {
                    href.to_string()
                } else {
                    format!(
                        "{}/{}",
                        base_url.trim_end_matches('/'),
                        href.trim_start_matches('/')
                    )
                };
                if !seen_urls.insert(full_url.clone()) {
                    continue;
                }

                let parsed = crate::parser::parse_anime_release(&title);
                results.push(TrackerTorrentEntry {
                    title,
                    url: full_url,
                    resolution: parsed.resolution,
                    group: parsed.release_group,
                    video_codec: parsed.video_codec,
                });
            }
        }

        results
    }

    /// Executes the multi-step upload to `animez.to` (or logs and returns immediately in `--dry-run` mode).
    pub async fn upload_release(
        &self,
        parsed: &ParsedRelease,
        mal: Option<&MalMetadata>,
        bundle: &PackageBundle,
        dry_run: bool,
    ) -> Result<UploadResult> {
        // Enforce AnimeZ upload rules (Section 6: minimum 6 screenshots for anime uploads)
        if bundle.screenshots.len() < 6 {
            anyhow::bail!(
                "Cannot upload to AnimeZ: Anime uploads strictly require a minimum of 6 screenshots (found {}). Aborting to comply with AnimeZ Upload Rules (Section 6).",
                bundle.screenshots.len()
            );
        }

        let animez_entry = self
            .lookup_anime_entry(&parsed.clean_title, parsed.year, parsed.season, mal.map(|m| m.mal_id))
            .await
            .ok()
            .flatten();

        let anime_id = animez_entry.as_ref().map(|(id, _)| *id);
        let animez_title = animez_entry.as_ref().map(|(_, t)| t.as_str());

        let torrent_name = crate::parser::format_animez_torrent_title(
            parsed,
            mal,
            animez_title,
        );

        if dry_run || self.config.general.dry_run {
            info!(
                title = %parsed.raw_title,
                formatted_name = %torrent_name,
                torrent = %bundle.torrent_path.display(),
                mediainfo = %bundle.mediainfo_path.display(),
                screenshots = bundle.screenshots.len(),
                mal_id = ?mal.map(|m| m.mal_id),
                "[DRY-RUN] Skipping live POST to animez.to; local package verified."
            );
            return Ok(UploadResult {
                success: true,
                dry_run: true,
                torrent_id: None,
                torrent_page_url: None,
                download_torrent_url: None,
                downloaded_tracker_torrent_path: None,
                uploaded_image_urls: Vec::new(),
            });
        }

        let base = self.config.animez.base_url.trim_end_matches('/');
        let upload_url = format!(
            "{base}/{}",
            self.config.animez.upload_path.trim_start_matches('/')
        );

        // Step 1: Fetch `/upload` page to extract fresh Laravel CSRF `_token` and form action URL
        let upload_page_html = self.get_html(&upload_url).await?;
        let csrf_token = Self::extract_csrf_token(&upload_page_html)
            .context("Failed to find Laravel CSRF `_token` on animez.to upload page")?;
        let post_url = Self::extract_upload_action_url(base, &upload_url, &upload_page_html);

        // Attempt importing or resolving AnimeZ anime entry using MAL URL directly
        let mut final_anime_id = anime_id;
        let mut final_anime_title = animez_title.map(|s| s.to_string());

        if let Some(m) = mal {
            let mal_url = format!("https://myanimelist.net/anime/{}", m.mal_id);
            match self.import_or_resolve_anime(&mal_url, &csrf_token, &upload_url).await {
                Ok(Some((aid, title))) => {
                    info!(
                        anime_id = aid,
                        title = %title,
                        mal_id = m.mal_id,
                        "Resolved AnimeZ anime entry via MAL URL import"
                    );
                    final_anime_id = Some(aid);
                    final_anime_title = Some(title);
                }
                Ok(None) => {}
                Err(e) => {
                    debug!(error = %e, mal_id = m.mal_id, "Could not import anime via MAL URL; using search candidate");
                }
            }
        }

        // Fallback: If anime entry is still unresolved, search Nyaa for canonical MAL/AniDB links
        if final_anime_id.is_none() {
            let mut search_candidates = Vec::new();
            if let Some(nyaa_view) = crate::parser::find_nyaa_view_url(&self.http, &parsed.raw_title).await {
                search_candidates.push(nyaa_view);
            }
            if let Some(nyaa_view) = crate::parser::find_nyaa_view_url(&self.http, &parsed.clean_title).await {
                if !search_candidates.contains(&nyaa_view) {
                    search_candidates.push(nyaa_view);
                }
            }
            for candidate in search_candidates {
                if let Ok(links) = crate::parser::resolve_external_links_from_url(&self.http, &candidate).await {
                    if let Some(ref m_url) = links.mal_url {
                        if let Ok(Some((aid, title))) = self.import_or_resolve_anime(m_url, &csrf_token, &upload_url).await {
                            info!(
                                anime_id = aid,
                                title = %title,
                                url = %m_url,
                                "Resolved AnimeZ anime entry via discovered MAL link from Nyaa"
                            );
                            final_anime_id = Some(aid);
                            final_anime_title = Some(title);
                            break;
                        }
                    }
                    if let Some(ref a_url) = links.anidb_url {
                        if let Ok(Some((aid, title))) = self.import_or_resolve_anime(a_url, &csrf_token, &upload_url).await {
                            info!(
                                anime_id = aid,
                                title = %title,
                                url = %a_url,
                                "Resolved AnimeZ anime entry via discovered AniDB link from Nyaa"
                            );
                            final_anime_id = Some(aid);
                            final_anime_title = Some(title);
                            break;
                        }
                    }
                }
            }
        }

        let torrent_name = crate::parser::format_animez_torrent_title(
            parsed,
            mal,
            final_anime_title.as_deref(),
        );

        // Step 2: Prepare multipart form
        let torrent_bytes = fs::read(&bundle.torrent_path).with_context(|| {
            format!(
                "Failed to read packaged torrent file: {}",
                bundle.torrent_path.display()
            )
        })?;
        let torrent_filename = bundle
            .torrent_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("release.torrent")
            .to_string();

        let torrent_part = Part::bytes(torrent_bytes)
            .file_name(torrent_filename)
            .mime_str("application/x-bittorrent")?;

        let description = Self::build_release_description(parsed, mal);
        let format_val = Self::detect_release_format(parsed);

        info!(
            raw_title = %parsed.raw_title,
            formatted_name = %torrent_name,
            "Submitting upload with formatted torrent title adhering to AnimeZ naming conventions"
        );

        let mut form = Form::new()
            .text("_token", csrf_token)
            .part("torrent", torrent_part)
            .text("name", torrent_name)
            .text("title", parsed.clean_title.clone())
            .text("format", format_val)
            .text("media_info", bundle.mediainfo_text.clone())
            .text("mediainfo", bundle.mediainfo_text.clone())
            .text("description", description)
            .text(
                "is_anonymous",
                if self.config.animez.anonymous {
                    "1"
                } else {
                    "0"
                },
            )
            .text(
                "anonymous",
                if self.config.animez.anonymous {
                    "1"
                } else {
                    "0"
                },
            );

        if let Some(aid) = final_anime_id {
            form = form.text("anime_id", aid.to_string());
        }
        if let Some(m) = mal {
            form = form.text("mal_id", m.mal_id.to_string());
        }
        if let Some(res) = &parsed.resolution {
            form = form.text("resolution", res.clone());
        }
        if let Some(src) = &parsed.source {
            form = form.text("source", src.clone());
        }
        if let Some(vc) = &parsed.video_codec {
            form = form.text("video_codec", vc.clone());
        }
        if let Some(ac) = &parsed.audio_codec {
            form = form.text("audio_codec", ac.clone());
        }
        if let Some(grp) = &parsed.release_group {
            form = form.text("release_group", grp.clone());
        }

        // Attach screenshots if the form expects screenshot uploads
        let has_screenshot_input = upload_page_html.contains("screenshots")
            || upload_page_html.contains("screenshot_")
            || upload_page_html.contains("images[]");

        if has_screenshot_input {
            let mut total_screenshot_bytes: usize = 0;
            for (idx, shot_path) in bundle.screenshots.iter().enumerate() {
                if shot_path.exists() {
                    let ext = shot_path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();

                    // If screenshot is an oversized legacy PNG (> 1MB), convert it to JPEG via ffmpeg
                    // so retried releases don't trigger HTTP 413 Payload Too Large.
                    let (final_path, mime) = if ext == "png" {
                        let is_large = fs::metadata(shot_path).map(|m| m.len() > 1_000_000).unwrap_or(false);
                        if is_large {
                            let jpg_path = shot_path.with_extension("jpg");
                            let conv = tokio::process::Command::new(&self.config.packager.ffmpeg_bin)
                                .arg("-y")
                                .arg("-i")
                                .arg(shot_path)
                                .arg("-q:v")
                                .arg("2")
                                .arg(&jpg_path)
                                .output()
                                .await;
                            if let Ok(c_out) = conv {
                                if c_out.status.success() && jpg_path.exists() {
                                    debug!(
                                        original = %shot_path.display(),
                                        converted = %jpg_path.display(),
                                        "Converted oversized PNG screenshot to JPEG before upload"
                                    );
                                    (jpg_path, "image/jpeg")
                                } else {
                                    (shot_path.clone(), "image/png")
                                }
                            } else {
                                (shot_path.clone(), "image/png")
                            }
                        } else {
                            (shot_path.clone(), "image/png")
                        }
                    } else if ext == "webp" {
                        (shot_path.clone(), "image/webp")
                    } else {
                        (shot_path.clone(), "image/jpeg")
                    };

                    let bytes = fs::read(&final_path)?;
                    total_screenshot_bytes += bytes.len();
                    let fname = final_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("screen_{idx}.jpg"));
                    let part = Part::bytes(bytes)
                        .file_name(fname)
                        .mime_str(mime)?;
                    form = form.part("screenshots[]", part);
                }
            }

            if total_screenshot_bytes > 7_000_000 {
                warn!(
                    total_bytes = total_screenshot_bytes,
                    "Total screenshot payload exceeds 7MB; upload may encounter HTTP 413 on strict proxies"
                );
            }
        }

        self.config.polite_sleep().await;
        let headers = self.build_headers(Some(&upload_url)).await?;

        info!(
            title = %parsed.raw_title,
            post_url = %post_url,
            "Submitting multipart release upload to animez.to"
        );
        let resp = self
            .http
            .post(&post_url)
            .headers(headers)
            .multipart(form)
            .send()
            .await
            .context("Failed to POST multipart upload to animez.to")?;

        self.flaresolverr
            .ingest_set_cookie_headers(resp.headers())
            .await;

        let mut final_url = resp.url().to_string();
        let status = resp.status();
        let mut body = resp.text().await.unwrap_or_default();

        if !status.is_success() && !status.is_redirection() {
            anyhow::bail!("Tracker upload failed with HTTP {status}: {body}");
        }

        if final_url.ends_with("/upload")
            || (final_url.contains("/upload") && !final_url.contains("/torrents/"))
        {
            let err_msg = Self::extract_form_errors(&body);
            anyhow::bail!("AnimeZ rejected upload form and returned to '{final_url}': {err_msg}");
        }

        let mut torrent_id_str = None;
        let is_draft = final_url.ends_with("/edit")
            || (final_url.contains("/torrents/") && final_url.contains("/edit"));

        let extracted_id = Self::extract_torrent_id_from_url(&final_url);

        if is_draft {
            let tid = match extracted_id {
                Some(id) => id,
                None => {
                    anyhow::bail!(
                        "Upload resulted in draft edit page but failed to parse torrent ID from '{final_url}'"
                    );
                }
            };
            torrent_id_str = Some(tid.to_string());

            info!(
                torrent_id = tid,
                edit_url = %final_url,
                "Draft torrent created on animez.to; commencing Step 2 (upload screenshots, anime lookup & publishing)"
            );

            // Fetch edit page HTML to extract CSRF token and verify form
            let edit_html = self.get_html(&final_url).await?;
            let edit_csrf_token = Self::extract_csrf_token(&edit_html)
                .context("Failed to find CSRF token on torrent edit page")?;

            // Upload screenshots via Dropzone endpoint
            let uploaded_shots = self
                .upload_screenshots(tid, &bundle.screenshots, &edit_csrf_token, &final_url)
                .await?;
            info!(
                torrent_id = tid,
                uploaded = uploaded_shots,
                total = bundle.screenshots.len(),
                "Screenshots uploaded to draft"
            );

            if uploaded_shots < 6 {
                anyhow::bail!(
                    "Draft torrent {tid} has only {uploaded_shots} screenshots successfully uploaded; AnimeZ upload rules (Section 6) strictly require at least 6 screenshots before publishing",
                );
            }

            // Resolve AnimeZ internal anime entry (ID and canonical title)
            let (pub_anime_id, pub_anime_title) = if let (Some(aid), Some(atitle)) = (final_anime_id, &final_anime_title) {
                (aid, atitle.clone())
            } else {
                let mut resolved = None;
                if let Some(m) = mal {
                    let mal_url = format!("https://myanimelist.net/anime/{}", m.mal_id);
                    if let Ok(Some(entry)) = self.import_or_resolve_anime(&mal_url, &edit_csrf_token, &final_url).await {
                        resolved = Some(entry);
                    }
                }
                if resolved.is_none() {
                    resolved = self
                        .lookup_anime_entry(&parsed.clean_title, parsed.year, parsed.season, mal.map(|m| m.mal_id))
                        .await?;
                }
                if resolved.is_none() {
                    let mut search_candidates = Vec::new();
                    if let Some(nyaa_view) = crate::parser::find_nyaa_view_url(&self.http, &parsed.raw_title).await {
                        search_candidates.push(nyaa_view);
                    }
                    if let Some(nyaa_view) = crate::parser::find_nyaa_view_url(&self.http, &parsed.clean_title).await {
                        if !search_candidates.contains(&nyaa_view) {
                            search_candidates.push(nyaa_view);
                        }
                    }
                    for candidate in search_candidates {
                        if let Ok(links) = crate::parser::resolve_external_links_from_url(&self.http, &candidate).await {
                            if let Some(ref m_url) = links.mal_url {
                                if let Ok(Some(entry)) = self.import_or_resolve_anime(m_url, &edit_csrf_token, &final_url).await {
                                    resolved = Some(entry);
                                    break;
                                }
                            }
                            if let Some(ref a_url) = links.anidb_url {
                                if let Ok(Some(entry)) = self.import_or_resolve_anime(a_url, &edit_csrf_token, &final_url).await {
                                    resolved = Some(entry);
                                    break;
                                }
                            }
                        }
                    }
                }
                match resolved {
                    Some(entry) => entry,
                    None => {
                        anyhow::bail!(
                            "Could not resolve AnimeZ anime ID for '{}' (mal_id: {:?}); draft {tid} not published",
                            parsed.clean_title,
                            mal.map(|m| m.mal_id)
                        );
                    }
                }
            };

            // Publish draft
            let (published_url, published_body) = self
                .publish_draft(
                    tid,
                    parsed,
                    mal,
                    bundle,
                    &edit_csrf_token,
                    pub_anime_id,
                    Some(&pub_anime_title),
                    &final_url,
                )
                .await?;

            final_url = published_url;
            body = published_body;
        } else if let Some(tid) = extracted_id {
            torrent_id_str = Some(tid.to_string());
        }

        let download_link = torrent_id_str
            .as_ref()
            .map(|tid| format!("{base}/torrents/{tid}/download"))
            .or_else(|| Self::extract_download_torrent_url(base, &final_url, &body));

        let downloaded_tracker_torrent_path = if let Some(dl_url) = &download_link {
            let dest_path = bundle.torrent_path.with_extension("animez.torrent");
            self.download_tracker_torrent(dl_url, &dest_path).await?;
            info!(
                path = %dest_path.display(),
                "Downloaded re-signed tracker .torrent for auto-reseeding"
            );
            Some(dest_path)
        } else {
            anyhow::bail!(
                "Upload finished at '{final_url}' but could not determine download URL for re-signed tracker torrent"
            );
        };

        Ok(UploadResult {
            success: true,
            dry_run: false,
            torrent_id: torrent_id_str,
            torrent_page_url: Some(final_url),
            download_torrent_url: download_link,
            downloaded_tracker_torrent_path,
            uploaded_image_urls: Vec::new(),
        })
    }

    /// Uploads screenshots to the draft torrent via Dropzone endpoint (`POST /torrents/{id}/screenshots`).
    pub async fn upload_screenshots(
        &self,
        torrent_id: u64,
        screenshots: &[PathBuf],
        csrf_token: &str,
        referer: &str,
    ) -> Result<usize> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let url = format!("{base}/torrents/{torrent_id}/screenshots");
        let mut count = 0;

        for (idx, shot_path) in screenshots.iter().enumerate() {
            if !shot_path.exists() {
                continue;
            }

            let ext = shot_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();

            // If screenshot is an oversized legacy PNG (> 1MB), convert to JPEG
            let (final_path, mime) = if ext == "png" {
                let is_large = fs::metadata(shot_path).map(|m| m.len() > 1_000_000).unwrap_or(false);
                if is_large {
                    let jpg_path = shot_path.with_extension("jpg");
                    let conv = tokio::process::Command::new(&self.config.packager.ffmpeg_bin)
                        .arg("-y")
                        .arg("-i")
                        .arg(shot_path)
                        .arg("-q:v")
                        .arg("2")
                        .arg(&jpg_path)
                        .output()
                        .await;
                    if let Ok(c_out) = conv {
                        if c_out.status.success() && jpg_path.exists() {
                            (jpg_path, "image/jpeg")
                        } else {
                            (shot_path.clone(), "image/png")
                        }
                    } else {
                        (shot_path.clone(), "image/png")
                    }
                } else {
                    (shot_path.clone(), "image/png")
                }
            } else if ext == "webp" {
                (shot_path.clone(), "image/webp")
            } else {
                (shot_path.clone(), "image/jpeg")
            };

            let bytes = fs::read(&final_path)?;
            let fname = final_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("screenshot_{idx}.jpg"));

            let part = Part::bytes(bytes)
                .file_name(fname)
                .mime_str(mime)?;

            let form = Form::new().part("screenshot", part);

            self.config.polite_sleep().await;
            let headers = self.build_screenshot_headers(csrf_token, Some(referer)).await?;

            let resp = self
                .http
                .post(&url)
                .headers(headers)
                .multipart(form)
                .send()
                .await
                .with_context(|| format!("Failed to POST screenshot {} to {url}", shot_path.display()))?;

            let status = resp.status();
            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                warn!(
                    status = %status,
                    body = %err_body,
                    path = %final_path.display(),
                    "Failed to upload screenshot to tracker draft"
                );
            } else {
                count += 1;
                debug!(
                    torrent_id = torrent_id,
                    index = idx + 1,
                    total = screenshots.len(),
                    "Successfully uploaded screenshot to draft"
                );
            }
        }

        Ok(count)
    }

    /// Extracts the numeric torrent ID from a URL (e.g. `https://animez.to/torrents/36128` or `/torrents/36128-slug`).
    pub fn extract_torrent_id_from_url(url: &str) -> Option<u64> {
        if let Some(pos) = url.find("/torrents/") {
            let after = &url[pos + "/torrents/".len()..];
            after
                .split('/')
                .next()
                .and_then(|s| s.split('-').next())
                .and_then(|id_str| id_str.parse::<u64>().ok())
        } else {
            None
        }
    }

    /// Publishes a draft torrent on `animez.to` by submitting PATCH /torrents/{id} with is_draft = 0.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_draft(
        &self,
        torrent_id: u64,
        parsed: &ParsedRelease,
        mal: Option<&MalMetadata>,
        bundle: &PackageBundle,
        csrf_token: &str,
        anime_id: u64,
        anime_title: Option<&str>,
        edit_page_url: &str,
    ) -> Result<(String, String)> {
        let base = self.config.animez.base_url.trim_end_matches('/');
        let publish_url = format!("{base}/torrents/{torrent_id}");

        let torrent_name = crate::parser::format_animez_torrent_title(
            parsed,
            mal,
            anime_title,
        );

        let description = Self::build_release_description(parsed, mal);
        let format_val = Self::detect_release_format(parsed);
        let resolution_val = Self::detect_resolution(parsed);
        let rip_type_val = Self::detect_rip_type(parsed);
        let (coll_type, ep_start, ep_end) = Self::detect_collection_type(parsed);

        let media_info_text = if !bundle.mediainfo_text.trim().is_empty() {
            bundle.mediainfo_text.clone()
        } else {
            format!(
                "General\nComplete name : {}\nFormat : Matroska\n\nVideo\nResolution : {}\nCodec : {}\n\nAudio\nCodec : {}\n",
                parsed.raw_title,
                resolution_val,
                parsed.video_codec.as_deref().unwrap_or("HEVC"),
                parsed.audio_codec.as_deref().unwrap_or("AAC"),
            )
        };

        let mut form = Form::new()
            .text("_token", csrf_token.to_string())
            .text("_method", "PATCH")
            .text("is_draft", "0")
            .text("anime_id", anime_id.to_string())
            .text("name", torrent_name)
            .text("format", format_val)
            .text("resolution", resolution_val)
            .text("rip_type", rip_type_val)
            .text("collection_type", coll_type.to_string())
            .text("media_info", media_info_text)
            .text("description", description)
            .text("is_adult", "0")
            .text("is_censored", "0")
            .text(
                "is_anonymous",
                if self.config.animez.anonymous {
                    "1"
                } else {
                    "0"
                },
            );

        if let Some(start) = ep_start {
            form = form.text("episode_start", start);
        }
        if let Some(end) = ep_end {
            form = form.text("episode_end", end);
        }

        self.config.polite_sleep().await;
        let headers = self.build_headers(Some(edit_page_url)).await?;

        info!(
            torrent_id = torrent_id,
            publish_url = %publish_url,
            anime_id = anime_id,
            "Submitting PATCH publish request to animez.to"
        );

        let resp = self
            .http
            .post(&publish_url)
            .headers(headers)
            .multipart(form)
            .send()
            .await
            .context("Failed to submit PATCH publish request to animez.to")?;

        self.flaresolverr
            .ingest_set_cookie_headers(resp.headers())
            .await;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() && !status.is_redirection() {
            anyhow::bail!("Publishing draft torrent {torrent_id} failed with HTTP {status}: {body}");
        }

        if final_url.ends_with("/edit") {
            let err_msg = Self::extract_form_errors(&body);
            anyhow::bail!(
                "Publishing draft torrent {torrent_id} was rejected by tracker and redirected back to edit page: {err_msg}"
            );
        }

        info!(
            torrent_id = torrent_id,
            page_url = %final_url,
            "Successfully published draft release on animez.to!"
        );

        Ok((final_url, body))
    }

    /// Downloads the re-signed official `.torrent` from AnimeZ containing the user's tracker passkey/PID.
    pub async fn download_tracker_torrent(&self, download_url: &str, dest: &Path) -> Result<()> {
        self.config.polite_sleep().await;
        let headers = self.build_headers(None).await?;
        let resp = self
            .http
            .get(download_url)
            .headers(headers)
            .send()
            .await
            .with_context(|| format!("Failed to GET tracker torrent at {download_url}"))?;

        self.flaresolverr
            .ingest_set_cookie_headers(resp.headers())
            .await;

        if !resp.status().is_success() {
            anyhow::bail!("Tracker torrent download returned HTTP {}", resp.status());
        }

        let bytes = resp.bytes().await?;
        // Verify it looks like a valid bencoded dictionary (starts with 'd')
        if bytes.first().copied() != Some(b'd') {
            anyhow::bail!("Downloaded file from {download_url} is not a valid bencoded .torrent");
        }
        fs::write(dest, &bytes)?;
        Ok(())
    }

    pub fn extract_download_torrent_url(
        base_url: &str,
        page_url: &str,
        html: &str,
    ) -> Option<String> {
        let doc = Html::parse_document(html);
        if let Ok(sel) = Selector::parse(
            r#"a[href*="/download"], a[href$=".torrent"], a.btn-download-torrent"#,
        ) {
            for a in doc.select(&sel) {
                if let Some(href) = a.value().attr("href") {
                    if href.starts_with("http") {
                        return Some(href.to_string());
                    } else {
                        return Some(format!(
                            "{}/{}",
                            base_url.trim_end_matches('/'),
                            href.trim_start_matches('/')
                        ));
                    }
                }
            }
        }

        // If redirected to `/torrent/<id>` or `/torrents/<id>`, construct standard AvistaZ/AnimeZ download path:
        // /torrents/{id}/download
        if page_url.contains("/torrent/") || page_url.contains("/torrents/") {
            if let Some(id_seg) = page_url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .and_then(|s| s.split('-').next())
            {
                if id_seg.chars().all(|c| c.is_ascii_digit()) {
                    return Some(format!(
                        "{}/torrents/{id_seg}/download",
                        base_url.trim_end_matches('/')
                    ));
                }
            }
        }

        None
    }

    fn build_release_description(parsed: &ParsedRelease, mal: Option<&MalMetadata>) -> String {
        let mut lines = Vec::new();
        if let Some(m) = mal {
            lines.push(format!("[b]Title:[/b] {}", m.title));
            if let Some(eng) = &m.title_english {
                lines.push(format!("[b]English Title:[/b] {eng}"));
            }
            lines.push(format!(
                "[b]MyAnimeList:[/b] https://myanimelist.net/anime/{}",
                m.mal_id
            ));
            if let Some(syn) = &m.synopsis {
                lines.push(String::new());
                lines.push(syn.clone());
            }
            lines.push(String::new());
        }
        lines.push(format!("[b]Release:[/b] {}", parsed.raw_title));
        if let Some(res) = &parsed.resolution {
            lines.push(format!("[b]Resolution:[/b] {res}"));
        }
        if let Some(grp) = &parsed.release_group {
            lines.push(format!("[b]Release Group:[/b] {grp}"));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_csrf_and_inspect_form_html() {
        let sample_html = r#"
        <!DOCTYPE html>
        <html>
        <head>
            <title>Upload Torrent - AnimeZ</title>
            <meta name="csrf-token" content="meta_csrf_999">
        </head>
        <body>
            <form id="upload-form" action="https://animez.to/upload" method="POST" enctype="multipart/form-data">
                <input type="hidden" name="_token" value="form_csrf_12345">
                <input type="file" name="torrent" id="torrent_file" required>
                <input type="text" name="name" placeholder="Release Name" required>
                <select name="resolution" required>
                    <option value="1080p" selected>1080p</option>
                    <option value="2160p">2160p</option>
                    <option value="720p">720p</option>
                </select>
                <textarea name="mediainfo" id="mediainfo"></textarea>
            </form>
        </body>
        </html>
        "#;

        let token = AnimezClient::extract_csrf_token(sample_html);
        assert_eq!(token.as_deref(), Some("form_csrf_12345"));

        let report = AnimezClient::parse_upload_form_html("https://animez.to/upload", sample_html);
        assert_eq!(report.page_title, "Upload Torrent - AnimeZ");
        assert!(report.csrf_token_present);
        assert_eq!(report.forms.len(), 1);
        assert_eq!(report.forms[0].fields.len(), 5);
        assert_eq!(report.forms[0].fields[3].options.len(), 3);
    }

    #[test]
    fn test_extract_turnstile_sample() {
        let sample = r#"
        <form action="/login" method="POST">
            <input type="hidden" name="_token" value="abc123csrf">
            <input type="hidden" name="cf-turnstile-response" id="cf-chl-widget-xyz_response" value="1.test_token_730_bytes_data">
        </form>
        "#;
        let token = AnimezClient::extract_turnstile_token(sample);
        assert_eq!(token.as_deref(), Some("1.test_token_730_bytes_data"));
    }

    #[test]
    fn test_detect_format_rip_type_resolution() {
        let parsed = crate::parser::parse_anime_release(
            "[Erai-raws] Ranma 1/2 (2024) 3rd Season - 07 [1080p NF WEBRip HEVC AAC][MultiSub][44A0F064]",
        );
        assert_eq!(AnimezClient::detect_release_format(&parsed), "TV");
        assert_eq!(AnimezClient::detect_resolution(&parsed), "1080p");
        assert_eq!(AnimezClient::detect_rip_type(&parsed), "WEBRip");
        let (coll, ep_start, _) = AnimezClient::detect_collection_type(&parsed);
        assert_eq!(coll, "single");
        assert_eq!(ep_start.as_deref(), Some("7"));
    }

    #[test]
    fn test_parse_torrent_search_html_with_torrent_link_class() {
        let sample_html = r#"
        <div>
            <a class="torrent-link" href="https://animez.to/torrents/36122">
                [Erai-raws] Ranma 1/2 (2024) 3rd Season - 07 [1080p NF WEBRip HEVC AAC][MultiSub][44A0F064]
            </a>
            <a class="torrent-link" href="https://animez.to/torrents/36122/edit">
                Edit Torrent
            </a>
            <a class="view-torrent" href="/torrents/100">
                [SubsPlease] Bleach - 10 [1080p]
            </a>
        </div>
        "#;
        let entries = AnimezClient::parse_torrent_search_html("https://animez.to", sample_html);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].url, "https://animez.to/torrents/36122");
        assert_eq!(entries[0].resolution.as_deref(), Some("1080p"));
        assert_eq!(entries[1].url, "https://animez.to/torrents/100");
    }

    #[test]
    fn test_normalize_title_and_extract_errors() {
        assert_eq!(
            AnimezClient::normalize_title_for_lookup("Ranma 1/2 (2024)!"),
            "ranma 12 2024"
        );
        let err_html = r#"<div class="alert alert-danger">At least 3 screenshots required to publish.</div>"#;
        assert_eq!(
            AnimezClient::extract_form_errors(err_html),
            "At least 3 screenshots required to publish."
        );
    }

    #[tokio::test]
    async fn test_upload_release_fails_if_screenshots_under_6() {
        let config = AppConfig::default();
        let flaresolverr = crate::flaresolverr::FlareSolverrClient::new(config.clone()).unwrap();
        let client = AnimezClient::new(config, flaresolverr).unwrap();

        let parsed = crate::parser::parse_anime_release("[SubsPlease] Title - 01 (1080p).mkv");
        let bundle = PackageBundle {
            torrent_path: PathBuf::from("dummy.torrent"),
            torrent_info_hash: "hash".to_string(),
            mediainfo_path: PathBuf::from("dummy.txt"),
            mediainfo_text: "dummy".to_string(),
            screenshots: vec![PathBuf::from("1.jpg"), PathBuf::from("2.jpg")], // only 2 screenshots
            content_path: PathBuf::from("dummy.mkv"),
            primary_video_path: PathBuf::from("dummy.mkv"),
            total_size_bytes: 100,
            file_count: 1,
            piece_length: 262144,
        };

        let res = client.upload_release(&parsed, None, &bundle, false).await;
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("strictly require a minimum of 6 screenshots"));
    }

    #[test]
    fn test_extract_torrent_id_from_url() {
        assert_eq!(
            AnimezClient::extract_torrent_id_from_url("https://animez.to/torrents/36128"),
            Some(36128)
        );
        assert_eq!(
            AnimezClient::extract_torrent_id_from_url("https://animez.to/torrents/36128-slug-title"),
            Some(36128)
        );
        assert_eq!(
            AnimezClient::extract_torrent_id_from_url("https://animez.to/torrents/36128/edit"),
            Some(36128)
        );
        assert_eq!(
            AnimezClient::extract_torrent_id_from_url("https://animez.to/upload"),
            None
        );
    }

    #[test]
    fn test_extract_download_torrent_url() {
        let base = "https://animez.to";
        let page_url = "https://animez.to/torrents/36128";
        let html_with_btn = r#"
            <div>
                <a class="btn btn-lg btn-primary" href="https://animez.to/torrents/36128/download">Download</a>
            </div>
        "#;
        assert_eq!(
            AnimezClient::extract_download_torrent_url(base, page_url, html_with_btn).as_deref(),
            Some("https://animez.to/torrents/36128/download")
        );

        // Fallback when HTML has no matching download button
        assert_eq!(
            AnimezClient::extract_download_torrent_url(base, page_url, "<html><body>No button</body></html>").as_deref(),
            Some("https://animez.to/torrents/36128/download")
        );
    }

    #[test]
    fn test_score_anime_candidate_seasons() {
        let s1 = AnimeSearchItem {
            id: 10654,
            title: "A Wild Last Boss Appeared!".to_string(),
            title_romaji: Some("Yasei no Last Boss ga Arawareta!".to_string()),
            title_native: None,
            year: Some(2025),
            status: Some("finished".to_string()),
            external_ids: None,
        };

        let s2 = AnimeSearchItem {
            id: 13495,
            title: "A Wild Last Boss Appeared! (2026)".to_string(),
            title_romaji: Some("Yasei no Last Boss ga Arawareta! (2026)".to_string()),
            title_native: None,
            year: Some(2026),
            status: Some("releasing".to_string()),
            external_ids: None,
        };

        // Looking for Season 2: S2 must beat S1 decisively
        let score_s1_for_s2 = AnimezClient::score_anime_candidate(
            &s1,
            "Yasei no Last Boss ga Arawareta",
            None,
            Some(2),
            None,
        );
        let score_s2_for_s2 = AnimezClient::score_anime_candidate(
            &s2,
            "Yasei no Last Boss ga Arawareta",
            None,
            Some(2),
            None,
        );
        assert!(
            score_s2_for_s2 > score_s1_for_s2,
            "Season 2 item must outscore Season 1 item when target_season is 2 (got S2={score_s2_for_s2}, S1={score_s1_for_s2})"
        );
        assert!(score_s2_for_s2 > 0);
        assert_eq!(score_s1_for_s2, -999, "Season 1 item must be completely disqualified for Season 2 target");

        // Looking for Season 1: S1 must beat S2 decisively
        let score_s1_for_s1 = AnimezClient::score_anime_candidate(
            &s1,
            "Yasei no Last Boss ga Arawareta",
            None,
            Some(1),
            None,
        );
        let score_s2_for_s1 = AnimezClient::score_anime_candidate(
            &s2,
            "Yasei no Last Boss ga Arawareta",
            None,
            Some(1),
            None,
        );
        assert!(
            score_s1_for_s1 > score_s2_for_s1,
            "Season 1 item must outscore Season 2 item when target_season is 1 (got S1={score_s1_for_s1}, S2={score_s2_for_s1})"
        );
        assert!(score_s1_for_s1 > 0);
        assert_eq!(score_s2_for_s1, -999, "Season 2 item must be completely disqualified for Season 1 target");
    }
}
