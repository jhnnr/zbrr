# CLAUDE.md

This file provides guidance to Claude Code (and other AI coding assistants) when working with the Rust codebase in `F:\Dev Stuff\AnimeZ`.

Read [AGENTS.md](./AGENTS.md) first for the full system architecture, pipeline state machine, and private-tracker safety rules.

---

## Quick Context

- **Language & Stack:** Rust (2021 edition, `tokio` async runtime, `reqwest`, `rusqlite` bundled, `clap`, `scraper`, `serde_bencode`, `rayon`, `sha1`).
- **Goal:** Automate grabbing anime releases (`.torrent` & magnet links) from a **Prowlarr** server, downloading them in **qBittorrent**, packaging them (parallel `.torrent` creation, MediaInfo via `mediainfo` CLI, PNG screenshots via `ffmpeg`, metadata via filename parser + Jikan/MAL), uploading to **animez.to** via direct HTTP + **FlareSolverr** + session cookies, and injecting the tracker's re-signed `.torrent` back into qBittorrent for immediate seeding.
- **OS Environment:** Windows (`F:\Dev Stuff\AnimeZ`). Always use `std::path::{Path, PathBuf}` so Windows drive letters and paths are handled cleanly across qBittorrent, `ffmpeg`, and `serde_bencode`.

---

## Common Commands

```powershell
# Build & check
cargo check
cargo clippy --all-targets
cargo test

# Build optimized release binary
cargo build --release

# Verify external binaries (ffmpeg, ffprobe, mediainfo) and service connectivity
cargo run -- doctor

# Dump & inspect animez.to upload form fields via FlareSolverr + Cookies (defaults to ~/.zbrr/upload_form_schema.json)
cargo run -- inspect-form

# Test creating a .torrent + MediaInfo + Screenshots on a local file/folder
cargo run -- package-local "F:\Downloads\Some.Anime.Episode.mkv"

# Run a single poll-and-package cycle in dry-run mode (no live tracker upload)
cargo run -- once --dry-run

# Run a single live cycle
cargo run -- once

# Start the continuous background polling daemon
cargo run -- run

# List tracked releases in SQLite state DB (alias: `list`)
cargo run -- status
cargo run -- status --state FAILED

# Retry a failed release by ID (add `--now` to execute immediately)
cargo run -- retry <id>
cargo run -- retry <id> --now
```

---

## Configuration & Environment Settings (`.env`)

Copy `.env.example` to `.env` (gitignored). All runtime settings and secrets are read directly from `.env`:

```dotenv
ZBRR_PROWLARR_URL=http://localhost:9696
ZBRR_PROWLARR_API_KEY=your_prowlarr_api_key_here
ZBRR_PROWLARR_INDEXERS=Nyaa.si
ZBRR_PROWLARR_CATEGORIES=5070

ZBRR_QBITTORRENT_URL=http://localhost:8080
ZBRR_QBITTORRENT_API_KEY=your_qbittorrent_api_key_here

ZBRR_FLARESOLVERR_URL=http://localhost:8191/v1

ZBRR_ANIMEZ_BASE_URL=https://animez.to
ZBRR_ANIMEZ_USERNAME=your_animez_username_here
ZBRR_ANIMEZ_PASSWORD=your_animez_password_here
ZBRR_ANIMEZ_PID=your_animez_pid_here
```

- When `ZBRR_ANIMEZ_PID` is configured, `zbrr` automatically constructs the tracker announce URL `https://tracker.animez.to/<PID>/announce`.
- When `ZBRR_ANIMEZ_USERNAME` and `ZBRR_ANIMEZ_PASSWORD` are configured, `AnimezClient` automatically authenticates against `/login` (using FlareSolverr if Cloudflare challenges) whenever `~/.zbrr/cookies.json` has no active session or the session expires.
- All persistent runtime state (`state.db`, `cookies.json`, `upload_form_schema.json`, `staging/`) is stored under `~/.zbrr` by default (with auto-migration from legacy `./data/`).

---

## Rust Engineering Guidelines & Conventions

1. **Bencode `info` Dictionary Key Ordering (`torrent_builder.rs`):**
   - BitTorrent specifications require dictionary keys in Bencode to be sorted lexicographically by raw byte string.
   - In `serde_bencode`, struct fields serialize in the order they are declared in the Rust `struct`. Therefore, declare fields in `TorrentInfo` and `TorrentRoot` in **strict alphabetical order**:
     - `TorrentInfo`: `files`, `length`, `name`, `piece length` (`#[serde(rename = "piece length")]`), `pieces` (`serde_bytes::ByteBuf`), `private`, `source`.
   - Note that multi-file torrents on Windows must normalize path separators in `files[].path` to forward-slash components (each directory/filename segment as a separate `String` in `Vec<String>`).

2. **Windows Path & Subprocess Safety (`packager.rs`):**
   - Always invoke `tokio::process::Command::new("ffmpeg")` / `ffprobe` / `mediainfo` with `.arg(&path)`—never invoke via `cmd /C` or shell interpolation, as anime filenames routinely contain brackets `[SubsPlease]`, parentheses `(1080p)`, spaces, and Unicode characters.
   - Support optional path mapping (`ZBRR_QBITTORRENT_REMOTE_PATH_PREFIX` -> `ZBRR_QBITTORRENT_LOCAL_PATH_PREFIX` in `.env`) in case qBittorrent runs in Docker/WSL or on a NAS while `zbrr` runs natively on Windows.

3. **Cloudflare, FlareSolverr & Tracker Session Reuse (`flaresolverr.rs` & `animez_client.rs`):**
   - **CRITICAL:** `cf_clearance` cookies are bound to the **exact `User-Agent` string** and IP address that solved the challenge. Whenever FlareSolverr solves or refreshes a session, save both `cookies` and `user_agent` to `~/.zbrr/cookies.json`, and configure the `reqwest::Client` (or request headers) with that exact `User-Agent`.
   - Always extract the fresh Laravel CSRF token (`input[name="_token"]` or `meta[name="csrf-token"]` via `scraper::Selector`) before submitting login or upload `POST` requests to `animez.to`.
   - If a request to `animez.to` is redirected to the login page and `username` + `password` are configured, automatically perform the `/login` flow and persist the updated `animez_session` and `remember_web_*` cookies to `~/.zbrr/cookies.json`.

4. **Idempotency & Fault Isolation (`pipeline.rs` & `db.rs`):**
   - Each release moves through the SQLite state machine (`DISCOVERED` -> `DUPE_CHECKED` -> `DOWNLOADING` -> `PACKAGING` -> `UPLOADING` -> `SEEDING`).
   - Isolate per-release errors with `anyhow::Result` so a single failed torrent marks that row as `FAILED` with `error_message` in SQLite without crashing the daemon loop.
   - Wrap blocking `rusqlite` or `rayon` hashing calls in `tokio::task::spawn_blocking` when called from async contexts.

---

## Implementation Modules Overview

1. **`config.rs` & `models.rs`:** Strongly typed configuration loaded from `.env` / environment variables and domain state machine types.
2. **`db.rs`:** Bundled `rusqlite` state store tracking releases, infohashes, MAL ID cache, and error logs.
3. **`prowlarr.rs` & `parser.rs`:** Prowlarr API/Torznab poller, regex/group filtering, anime release filename parser, and Jikan v4 MAL ID lookup.
4. **`flaresolverr.rs` & `animez_client.rs`:** FlareSolverr session manager, `animez.to` dupe checker, HTML form inspector (`inspect-form`), and multi-step multipart uploader.
5. **`qbittorrent.rs`:** qBittorrent WebUI v2 client (Bearer API key auth) for adding downloads, monitoring completion, and injecting re-signed tracker torrents (`skip_checking=true`).
6. **`torrent_builder.rs` & `packager.rs`:** Parallel SHA-1 piece hasher (`rayon`), `mediainfo` runner, and `ffmpeg` screenshot generator.
