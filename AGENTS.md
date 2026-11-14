# AGENTS.md — AnimeZ Automated RSS-to-Tracker Pipeline (Rust)

## 1. Project Overview

This repository (`zbrr`) is a high-performance, single-binary **Rust** automation service and CLI tool that monitors a **Prowlarr** server (via REST API / Torznab RSS) for anime releases (`.torrent` links and magnet links), performs duplicate checks against **animez.to** (AvistaZ network codebase), downloads missing releases via **qBittorrent**, packages the completed media (multi-threaded Bencode `.torrent` creation, MediaInfo extraction, FFmpeg screenshots, and MAL/AniDB metadata), uploads the release to **animez.to** using direct HTTP requests backed by **FlareSolverr** and session cookies, and automatically injects the tracker's re-signed `.torrent` back into qBittorrent for immediate seeding.

---

## 2. Architecture & State Machine

Every release discovered from Prowlarr is tracked in a local **SQLite** database (`~/.zbrr/state.db`) and progresses through a strict, idempotent state machine:

```
DISCOVERED -> DUPE_CHECKED -> DOWNLOADING -> PACKAGING -> UPLOADING -> SEEDING
                   |               |             |            |
                   v               v             v            v
                SKIPPED          FAILED        FAILED       FAILED
```

### Pipeline Stages
1. **Discovery (`prowlarr.rs`)**
   - Resolves target indexers dynamically by name (`indexers = ["Nyaa.si"]`) or numeric ID (`indexer_ids`), automatically detecting the AnimeZ tracker ID and excluding it from source polling (`exclude_indexers = ["AnimeZ"]`).
   - Polls configured Prowlarr indexers via `/api/v1/search` (JSON) or Torznab RSS (`/1/api?t=rss` via `quick-xml`) with configurable categories and search queries.
   - Filters items by include/exclude regexes, release group whitelist, resolution, source media (`allowed_sources`), video codec (`allowed_codecs`), indexer exclusion, minimum seeders, and size bounds.
   - Deduplicates by GUID, infohash, and normalized release title in SQLite.
2. **Pre-Download Dupe Check (`dupe_checker.rs` & `parser.rs`)**
   - Parses the anime release filename (extracting anime title, season/episode, resolution, source, video/audio codecs, and release group).
   - Resolves the canonical **MyAnimeList (MAL) / AniDB / TVDB / IMDB ID** via Jikan API v4 (with local SQLite caching) or `animez.to`'s internal search autocomplete endpoint.
   - Checks `animez.to` (via Prowlarr indexer query or direct HTTP search on `animez.to/torrents`) to verify whether the same episode/pack + resolution + group already exists. If it exists, marks status as `SKIPPED`.
3. **Download Management (`qbittorrent.rs`)**
   - Authenticates statelessly with qBittorrent WebUI API using a Bearer API key (`Authorization: Bearer <api_key>`) and adds the `.torrent` file or magnet URI under a staging category (default: `animez-staging`) and tag (`animez-auto`).
   - Polls `/api/v2/torrents/info` until progress reaches `100%` (`uploading` / `stalledUP` / `pausedUP` / `queuedUP`).
4. **Media Packaging (`packager.rs` & `torrent_builder.rs`)**
   - Inspects the downloaded files on disk (ignoring OS junk files).
   - **MediaInfo**: Invokes the `mediainfo` CLI binary to extract clean raw text MediaInfo from the primary `.mkv`/`.mp4` file, stripping local filesystem paths from `Complete name`.
   - **Screenshots**: Invokes `ffprobe` for duration and `ffmpeg` to extract 3–4 high-quality JPEG screenshots (with WebP/PNG support) at evenly spaced timestamps (e.g., 15%, 35%, 55%, 75% of duration), keeping multipart upload payload under tracker HTTP 413 limits.
   - **Fast Multi-Threaded Torrent Creation (`torrent_builder.rs`)**:
     - Uses `rayon` + `sha1` + `serde_bencode` to hash pieces in parallel across CPU cores and generate a brand-new `.torrent` file directly from the downloaded disk payload with:
       - `info.private = 1`
       - `info.source = "animez.to"`
       - Tracker `announce` URL from configuration (auto-generated as `https://tracker.animez.to/<pid>/announce` from `ZBRR_ANIMEZ_PID` / `animez.pid`)
       - Automatic power-of-two piece size selection (`256 KiB` to `16 MiB` based on total payload size)
5. **Tracker Authentication & Upload (`animez_client.rs` & `flaresolverr.rs`)**
   - Uses `reqwest` (with `cookies`, `multipart`, `gzip`, and `rustls-tls`) synchronized with session cookies (`animez_session`, `XSRF-TOKEN`, `remember_web_*`, `cf_clearance`) and exact `User-Agent` matching.
   - Calls **FlareSolverr** (`POST http://localhost:8191/v1`) only when initializing a session or when Cloudflare returns a `403`/`503` challenge, caching valid cookies and `User-Agent` on disk (`~/.zbrr/cookies.json`).
   - Automatically logs into `animez.to/login` using `ZBRR_ANIMEZ_USERNAME` and `ZBRR_ANIMEZ_PASSWORD` (extracting the Laravel `_token` CSRF token) whenever the cached session is missing or expired, persisting the resulting session cookies to `~/.zbrr/cookies.json`.
   - Executes `animez.to`'s multi-step Laravel upload flow (extracting CSRF `_token` from HTML via `scraper`), uploading screenshots, MediaInfo, metadata tags, and the `.torrent` file.
6. **Auto-Reseed (`qbittorrent.rs`)**
   - Downloads the newly published `.torrent` file returned by `animez.to` (which carries the tracker's official PID/passkey and infohash).
   - Injects that `.torrent` into qBittorrent pointing to the exact same `save_path` with `skip_checking=true` and category `animez-seeding`.

---

## 3. Tech Stack & Rust Crates

- **Edition & Runtime:** Rust 2021 / 2024 (`tokio` async runtime)
- **CLI & Config:** `clap` (derive), `serde`, `serde_json`, `serde_yaml` (or `toml`), `dotenvy`
- **HTTP & HTML Scraping:** `reqwest` (`json`, `multipart`, `cookies`, `stream`), `scraper` (HTML/CSS selector parsing for Laravel CSRF & form inspection), `quick-xml` (Torznab XML parsing), `url`
- **Torrent & Hashing:** `serde_bencode`, `serde_bytes`, `sha1`, `rayon` (parallel piece hashing), `walkdir`
- **Anime Filename Parsing:** Custom Anitopy-style tokenizer / `regex` parser for `[Group] Title - S01E01 (1080p) [CRC].mkv`
- **Database:** `rusqlite` (with `bundled` feature so SQLite compiles directly into the binary with zero system DLL dependencies)
- **Logging & Error Handling:** `tracing`, `tracing-subscriber`, `anyhow`, `thiserror`, `chrono`

---

## 4. Target Directory Layout

```text
F:\Dev Stuff\AnimeZ\
├── AGENTS.md
├── CLAUDE.md
├── Cargo.toml
├── .env.example
├── .gitignore
├── src/
│   ├── main.rs                # Entrypoint & CLI dispatcher
│   ├── cli.rs                 # Clap CLI definitions (run, once, inspect-form, doctor, package-local, status/list, retry)
│   ├── config.rs              # Config structs, tilde expansion & validation (.env + ZBRR_* env overrides)
│   ├── models.rs              # Domain structs & ReleaseState enum
│   ├── db.rs                  # rusqlite repository, WAL setup & schema migrations
│   ├── prowlarr.rs            # Prowlarr API / Torznab RSS client
│   ├── parser.rs              # Anime release filename parser + Jikan (MAL) ID resolver
│   ├── dupe_checker.rs        # Pre-download & pre-upload duplicate detection on animez.to
│   ├── qbittorrent.rs         # qBittorrent WebUI v2 API client (download & fast-resume seed)
│   ├── torrent_builder.rs     # Parallel Bencode .torrent creator (private=1, source="animez.to")
│   ├── packager.rs            # mediainfo & ffmpeg/ffprobe wrapper + staging coordinator
│   ├── flaresolverr.rs        # FlareSolverr REST client & cookie/User-Agent persistence
│   ├── animez_client.rs       # animez.to HTTP scraper, form inspector & multi-step uploader
│   └── pipeline.rs            # Async orchestrator loop connecting all stages
└── ~/.zbrr/                   # User data directory (auto-created in user home)
    ├── state.db               # SQLite database for release state & metadata cache
    ├── cookies.json           # Cached FlareSolverr / animez.to cookies & User-Agent
    ├── upload_form_schema.json# Dumped tracker upload form schema (inspect-form)
    └── staging/               # Generated .torrent files, MediaInfo .txt, and PNG screenshots
```

---

## 5. Critical Domain Rules & Safety Guardrails for Agents

> [!IMPORTANT]
> Private trackers enforce strict automation rules. Every agent working on this codebase MUST follow these invariants:

1. **Dry-Run First (`--dry-run`):**
   - The pipeline and CLI must always support a `--dry-run` flag that executes discovery, dupe-checking, downloading, and local packaging (`.torrent`, MediaInfo, screenshots) **without** sending the final upload `POST` to `animez.to`.
2. **Form Inspection Before Live Upload (`inspect-form`):**
   - Because `animez.to` (AvistaZ network Laravel codebase) can update its upload HTML form fields or dropdown option IDs, always maintain the `inspect-form` CLI subcommand. It fetches `https://animez.to/upload` via FlareSolverr + cookies and dumps all `<form>`, `<input>`, `<select>`, and `<textarea>` names/options to a local JSON report.
3. **Never Reuse Source `.torrent` Files:**
   - Source torrents from Prowlarr may originate from public trackers (e.g., Nyaa, SubsPlease) or magnets. Always build a brand-new `.torrent` in `torrent_builder.rs` from the downloaded files with `private = Some(1)` and `source = Some("animez.to")`.
   - Furthermore, per the BitTorrent Bencode specification, dictionary keys inside the `info` dictionary **must** be sorted lexicographically (`serde_bencode` handles struct fields in declaration order or via `BTreeMap`—ensure keys in `InfoDict` are in strict alphabetical order: `files`, `length`, `name`, `piece length`, `pieces`, `private`, `source`).
4. **Preserve Original Seeding & Re-Download Tracker Torrent:**
   - Never delete or overwrite the source torrent in qBittorrent if it is still seeding on another tracker. Add the newly downloaded `animez.to` `.torrent` as a separate torrent instance pointing to the same file path with `skip_checking=true`.
5. **FlareSolverr & Rate Limiting:**
   - Never call FlareSolverr on every single HTTP request. Cache the `cf_clearance` cookie and the exact `User-Agent` returned by FlareSolverr in `~/.zbrr/cookies.json`, and use direct `reqwest` calls until a session expires or returns HTTP `403`/`503`.
   - Enforce polite delays (`2–5 seconds` jitter) between HTTP requests to `animez.to` and Jikan API.
6. **Zero Hardcoded Secrets:**
   - Never commit AnimeZ credentials (`username`, `password`, `pid`), session cookies, announce URLs, Prowlarr API keys, or qBittorrent API keys. All secrets must load from `.env` (which must be in `.gitignore`).
