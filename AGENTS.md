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
0. **Staging Adoption & Active Seeding Priority (`pipeline.rs`)**
   - Inspects qBittorrent before polling Prowlarr. Torrents already in `animez-staging` that are completed or untracked are adopted and prioritized straight into the packaging pipeline.
   - Inspects `animez-seeding` to identify anime series currently seeding and prioritizes discovery/downloads for new episodes of those ongoing shows.
1. **Discovery (`prowlarr.rs`)**
   - Resolves target indexers dynamically by name (`indexers = ["Nyaa.si"]`) or numeric ID (`indexer_ids`), automatically detecting the AnimeZ tracker ID and excluding it from source polling (`exclude_indexers = ["AnimeZ"]`).
   - Polls configured Prowlarr indexers via `/api/v1/search` (JSON) or Torznab RSS (`/1/api?t=rss` via `quick-xml`) with configurable categories and search queries.
   - Filters items by include/exclude regexes, release group whitelist, resolution, source media (`allowed_sources`), video codec (`allowed_codecs`), indexer exclusion, minimum seeders, and size bounds.
   - Deduplicates by GUID, infohash, and normalized release title in SQLite.
2. **Pre-Download Dupe Check & Catch-Up Batch Logic (`dupe_checker.rs` & `parser.rs`)**
   - Parses the anime release filename (extracting anime title, season/episode, resolution, source, video/audio codecs, and release group).
   - Scrapes the canonical **MyAnimeList (MAL) link** directly from Nyaa's `Information` section (following Erai-raws anime list pages) or resolves via Jikan API v4 (with local SQLite caching).
   - **AnimeZ Upload Rules (Section 3)**: Checks whether episode `N > 1` is standalone. If no prior sequence exists in local DB or tracker profile, searches Prowlarr for missing prior episodes (`01 ~ N-1`), downloads the full set, and schedules a complete catch-up batch (`01-N`).
   - Checks `animez.to` (via Prowlarr indexer query or direct HTTP search on `animez.to/torrents`) to verify whether the same episode/pack + resolution + group already exists. If it exists, marks status as `SKIPPED`.
3. **Download Management (`qbittorrent.rs`)**
   - Authenticates statelessly with qBittorrent WebUI API using a Bearer API key (`Authorization: Bearer <api_key>`) and adds the `.torrent` file or magnet URI under a staging category (default: `animez-staging`) and tag (`animez-auto`).
   - Polls `/api/v2/torrents/info` until progress reaches `100%` (`uploading` / `stalledUP` / `pausedUP` / `queuedUP`).
4. **Media Packaging (`packager.rs` & `torrent_builder.rs`)**
   - Inspects the downloaded files on disk (ignoring OS junk files).
   - **MediaInfo**: Invokes the `mediainfo` CLI binary to extract clean raw text MediaInfo from the primary `.mkv`/`.mp4` file, stripping local filesystem paths from `Complete name`.
   - **Screenshots & Robust Seeking**:
     - Probes video duration via MediaInfo video track (`--Inform="Video;%Duration%"`) and ffprobe stream duration (`-select_streams v:0 -show_entries stream=duration`), taking `stream_dur.min(format_dur)` to eliminate trailing container duration overestimation.
     - Multi-stage seek fallbacks: fast input seek (`-ss <ts> -i <file>`), accurate output seek (`-i <file> -ss <ts>`), stepped earlier seeks (`90%`, `80%`, `70%`), and an expanded candidate position pool.
     - Strictly guarantees a minimum of 6 high-quality screenshots (maximum 9) per AnimeZ Upload Rules.
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
   - **Anime Entry Auto-Import**: If the anime is not yet indexed on AnimeZ, automatically creates and imports it via `POST /anime` using the canonical MAL ID and CSRF token.
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
F:\Dev Stuff\zbrr\
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
7. **Strict AnimeZ Naming Conventions:**
   - Never upload raw scene or indexer release filenames (e.g. `[Group] Title - S01E07 [1080p NF WEBRip HEVC AAC][MultiSub][CRC]`).
   - Titles must strictly conform to AnimeZ's naming guide (`https://animez.to/guides/how-to-properly-titlename-a-torrent`): `[AniDB Main Title] - [Episode] (Year) [Fansub] [Additional Tags...]` without resolutions, rip types, audio codecs, or CRC hashes, retaining only permitted tags (`[Dual Audio]`, `[Multi Audio]`, `[English Dubbed]`, `[RAW]`, `[HDR10]`, `[HEVC]`, `[10-bit]`).
   - Avoid double-year redundancy (e.g. `Ranma 1/2 (2024) - 01 (2026)` instead of `(2026) ... (2026)`).
8. **Anime Screenshot Requirement (Minimum 6):**
   - Anime uploads strictly require a minimum of 6 screenshots (maximum 9). The packager, pipeline, and uploader must validate that at least 6 screenshots exist and abort before uploading if fewer than 6 were generated.
   - Always probe true video track duration via MediaInfo and ffprobe stream duration (`stream_dur.min(format_dur)`) to avoid container duration overestimation past EOF.
   - Employ multi-stage seek fallbacks (fast input seek, accurate output seek, stepped earlier seeking) and an intermediate candidate pool so single timestamp misses never crash release packaging.
9. **Ongoing Series Catch-Up Rule & Batching:**
   - Per Section 3 of AnimeZ Upload Rules, standalone single episodes with `episode > 1` cannot be uploaded mid-season unless the user has established a prior sequence (having uploaded prior episodes in local DB or tracker profile) or uploads a catch-up batch of all episodes to date (`01~N`).
   - When no prior sequence exists, `zbrr` automatically searches Prowlarr for missing prior episodes (`01` through `N-1`), downloads the complete set, and packages them into a batch upload titled `[AniDB Title] - 01~N (Year) [Group] [Tags]`.
10. **Anime Entry Auto-Import & Season Matching:**
    - Never upload a Season 2 release under a Season 1 anime entry on `animez.to`. If the specific season entry is missing on AnimeZ, resolve the canonical MAL ID (scraped from Nyaa `Information` or via Jikan) and trigger AnimeZ's auto-import endpoint (`POST /anime`) with CSRF tokens before completing upload.
11. **Staging Priority & Untracked Torrent Adoption:**
    - Prioritize completed torrents already resting in `animez-staging` before starting new Prowlarr downloads. If an untracked completed torrent exists in staging, adopt it into SQLite and proceed directly to packaging.
