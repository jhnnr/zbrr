use crate::config::AppConfig;
use crate::models::PackageBundle;
use crate::torrent_builder::{build_private_torrent, is_ignored_os_file};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info};
use walkdir::WalkDir;

#[derive(Clone)]
pub struct Packager {
    config: AppConfig,
}

impl Packager {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    /// Verifies that `ffmpeg`, `ffprobe`, and `mediainfo` CLI binaries are installed and executable.
    pub async fn verify_binaries(&self) -> Vec<(String, Result<String>)> {
        let mut results = Vec::new();

        results.push((
            format!("ffmpeg ({})", self.config.packager.ffmpeg_bin),
            Self::probe_binary_version(&self.config.packager.ffmpeg_bin, "-version").await,
        ));
        results.push((
            format!("ffprobe ({})", self.config.packager.ffprobe_bin),
            Self::probe_binary_version(&self.config.packager.ffprobe_bin, "-version").await,
        ));
        results.push((
            format!("mediainfo ({})", self.config.packager.mediainfo_bin),
            Self::probe_binary_version(&self.config.packager.mediainfo_bin, "--Version").await,
        ));

        results
    }

    async fn probe_binary_version(bin: &str, version_arg: &str) -> Result<String> {
        let out = Command::new(bin)
            .arg(version_arg)
            .output()
            .await
            .with_context(|| format!("Failed to execute '{bin} {version_arg}' (is it in PATH?)"))?;

        if !out.status.success() {
            anyhow::bail!("'{bin} {version_arg}' exited with {}", out.status);
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let first_line = stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("OK")
            .trim()
            .to_string();
        Ok(first_line)
    }

    /// Packages a completed local file or folder into:
    /// 1. Private `.torrent` (`private=1`, `source="animez.to"`) via parallel `rayon` piece hashing
    /// 2. Clean `mediainfo.txt` (with local filesystem paths stripped)
    /// 3. Video screenshots (JPEG/WebP/PNG) via `ffprobe` + `ffmpeg`
    pub async fn package_release(
        &self,
        content_path: &Path,
        custom_output_dir: Option<&Path>,
        skip_screenshots: bool,
    ) -> Result<PackageBundle> {
        if !content_path.exists() {
            anyhow::bail!(
                "Cannot package release: path does not exist: {}",
                content_path.display()
            );
        }

        let release_stem = content_path
            .file_stem()
            .or_else(|| content_path.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("release");

        let safe_folder_name = sanitize_staging_name(release_stem);
        let bundle_dir = match custom_output_dir {
            Some(dir) => dir.to_path_buf(),
            None => self.config.staging_dir().join(&safe_folder_name),
        };
        fs::create_dir_all(&bundle_dir).with_context(|| {
            format!(
                "Failed to create staging bundle directory: {}",
                bundle_dir.display()
            )
        })?;

        let primary_video = Self::find_primary_video_file(content_path)?;
        info!(
            content = %content_path.display(),
            primary_video = %primary_video.display(),
            bundle_dir = %bundle_dir.display(),
            "Starting media packaging"
        );

        // 1. Build private .torrent in a blocking Rayon pool
        let torrent_path = bundle_dir.join(format!("{safe_folder_name}.torrent"));
        let content_owned = content_path.to_path_buf();
        let torrent_out_owned = torrent_path.clone();
        let announce_owned = self.config.animez.announce_url.clone();

        let torrent_built = tokio::task::spawn_blocking(move || {
            build_private_torrent(
                &content_owned,
                &torrent_out_owned,
                &announce_owned,
                "animez.to",
            )
        })
        .await
        .context("Torrent builder task panicked")??;

        info!(
            torrent = %torrent_built.torrent_path.display(),
            info_hash = %torrent_built.info_hash,
            pieces = torrent_built.piece_count,
            piece_len = torrent_built.piece_length,
            "Created private .torrent for animez.to"
        );

        // 2. Extract sanitized MediaInfo
        let mediainfo_text = self.extract_mediainfo(&primary_video).await?;
        let mediainfo_path = bundle_dir.join("mediainfo.txt");
        fs::write(&mediainfo_path, &mediainfo_text)?;

        // 3. Extract screenshots (minimum 6 for anime per AnimeZ Upload Rules Section 6)
        let screenshots = if skip_screenshots {
            Vec::new()
        } else {
            let shots = self.extract_screenshots(&primary_video, &bundle_dir).await?;
            if shots.len() < 6 {
                anyhow::bail!(
                    "Media packaging produced only {} screenshots; AnimeZ upload rules (Section 6) strictly require a minimum of 6 screenshots for anime uploads.",
                    shots.len()
                );
            }
            shots
        };

        Ok(PackageBundle {
            torrent_path: torrent_built.torrent_path,
            torrent_info_hash: torrent_built.info_hash,
            mediainfo_path,
            mediainfo_text,
            screenshots,
            content_path: content_path.to_path_buf(),
            primary_video_path: primary_video,
            total_size_bytes: torrent_built.total_size_bytes,
            file_count: torrent_built.file_count,
            piece_length: torrent_built.piece_length,
        })
    }

    /// Finds the primary video file (`.mkv`, `.mp4`, `.avi`, `.m2ts`, `.ts`) inside a file or folder.
    pub fn find_primary_video_file(content_path: &Path) -> Result<PathBuf> {
        if content_path.is_file() {
            return Ok(content_path.to_path_buf());
        }

        let mut candidates: Vec<(PathBuf, u64)> = Vec::new();
        for entry in WalkDir::new(content_path).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if is_ignored_os_file(p) {
                continue;
            }
            let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if matches!(
                ext.to_ascii_lowercase().as_str(),
                "mkv" | "mp4" | "avi" | "m2ts" | "ts"
            ) {
                let len = entry.metadata()?.len();
                candidates.push((p.to_path_buf(), len));
            }
        }

        if candidates.is_empty() {
            anyhow::bail!(
                "No video files (.mkv/.mp4/.avi/.m2ts) found inside {}",
                content_path.display()
            );
        }

        // Sort alphabetically first, but prefer files that are at least 25% of the largest file size
        // (avoids picking a tiny creditless OP/sample if it happens to sort first).
        let max_size = candidates.iter().map(|(_, sz)| *sz).max().unwrap_or(0);
        let min_main_size = max_size / 4;

        candidates.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((best, _)) = candidates.iter().find(|(_, sz)| *sz >= min_main_size) {
            return Ok(best.clone());
        }

        Ok(candidates[0].0.clone())
    }

    /// Invokes `mediainfo` directly with `.arg(video_path)` (never via shell) and strips full local directories.
    pub async fn extract_mediainfo(&self, video_path: &Path) -> Result<String> {
        let out = Command::new(&self.config.packager.mediainfo_bin)
            .arg(video_path)
            .output()
            .await
            .with_context(|| {
                format!(
                    "Failed to run '{}' on {}",
                    self.config.packager.mediainfo_bin,
                    video_path.display()
                )
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("mediainfo failed ({}): {}", out.status, stderr.trim());
        }

        let raw = String::from_utf8_lossy(&out.stdout);
        let filename_only = video_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mkv");

        Ok(sanitize_mediainfo_output(&raw, filename_only))
    }

    /// Invokes `ffprobe` for duration and `ffmpeg` to extract full-resolution PNG screenshots.
    /// Invokes `ffprobe` / `mediainfo` for duration and `ffmpeg` to extract full-resolution screenshots.
    pub async fn extract_screenshots(
        &self,
        video_path: &Path,
        output_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        let duration_secs = self.probe_video_duration(video_path).await.unwrap_or(1440.0);
        let target_count = self.config.packager.screenshot_count.max(6);

        let mut candidate_positions = self.config.packager.screenshot_positions.clone();
        if candidate_positions.is_empty() {
            candidate_positions = vec![0.10, 0.25, 0.40, 0.55, 0.70, 0.85];
        }
        // Build fallback pool with backup evenly spaced positions
        let fallback_pool = [0.18, 0.33, 0.48, 0.63, 0.78, 0.15, 0.30, 0.45, 0.60, 0.75, 0.80, 0.08, 0.50];
        for p in fallback_pool {
            if !candidate_positions.iter().any(|&cp| (cp - p).abs() < 0.03) {
                candidate_positions.push(p);
            }
        }

        let ext = match self.config.packager.screenshot_format.as_str() {
            "png" => "png",
            "webp" => "webp",
            _ => "jpg",
        };
        let quality_str = self.config.packager.screenshot_quality.to_string();

        let mut paths = Vec::with_capacity(target_count);
        for pct in candidate_positions {
            if paths.len() >= target_count {
                break;
            }
            let clamped = pct.clamp(0.01, 0.95);
            let mut ts_secs = duration_secs * clamped;
            if duration_secs > 1.0 {
                ts_secs = ts_secs.clamp(0.5, duration_secs - 0.5);
            } else {
                ts_secs = ts_secs.clamp(0.0, duration_secs);
            }
            let shot_num = paths.len() + 1;
            let shot_path = output_dir.join(format!("screenshot_{:02}.{ext}", shot_num));

            debug!(
                timestamp_secs = format!("{ts_secs:.2}"),
                pct = pct,
                shot_number = shot_num,
                output = %shot_path.display(),
                format = ext,
                "Extracting screenshot with ffmpeg"
            );

            if Self::extract_single_screenshot(
                &self.config.packager.ffmpeg_bin,
                video_path,
                &shot_path,
                ts_secs,
                ext,
                &quality_str,
            )
            .await
            {
                paths.push(shot_path);
            } else {
                debug!(
                    timestamp_secs = format!("{ts_secs:.2}"),
                    pct = pct,
                    "Screenshot extraction failed at timestamp; trying next candidate position"
                );
            }
        }

        if paths.len() < 6 {
            anyhow::bail!(
                "Failed to extract minimum 6 screenshots required by AnimeZ upload rules (extracted {})",
                paths.len()
            );
        }

        info!(
            count = paths.len(),
            "Extracted full-resolution screenshots successfully"
        );
        Ok(paths)
    }

    async fn extract_single_screenshot(
        ffmpeg_bin: &str,
        video_path: &Path,
        shot_path: &Path,
        ts_secs: f64,
        ext: &str,
        quality_str: &str,
    ) -> bool {
        // 1. Fast seek (-ss before -i)
        if Self::run_ffmpeg_capture(ffmpeg_bin, video_path, shot_path, ts_secs, true, ext, quality_str).await {
            return true;
        }

        // 2. Accurate output seek (-ss after -i) in case keyframe jumping failed
        if Self::run_ffmpeg_capture(ffmpeg_bin, video_path, shot_path, ts_secs, false, ext, quality_str).await {
            return true;
        }

        // 3. Fallback stepped slightly earlier in case ts_secs is near or past video EOF
        for step in [0.90, 0.80, 0.70] {
            let alt_ts = (ts_secs * step).max(0.1);
            if Self::run_ffmpeg_capture(ffmpeg_bin, video_path, shot_path, alt_ts, true, ext, quality_str).await {
                return true;
            }
            if Self::run_ffmpeg_capture(ffmpeg_bin, video_path, shot_path, alt_ts, false, ext, quality_str).await {
                return true;
            }
        }

        false
    }

    async fn run_ffmpeg_capture(
        ffmpeg_bin: &str,
        video_path: &Path,
        shot_path: &Path,
        ts_secs: f64,
        fast_seek: bool,
        ext: &str,
        quality_str: &str,
    ) -> bool {
        let _ = fs::remove_file(shot_path);

        let mut cmd = Command::new(ffmpeg_bin);
        cmd.arg("-y");

        if fast_seek {
            cmd.arg("-ss")
                .arg(format!("{ts_secs:.3}"))
                .arg("-i")
                .arg(video_path);
        } else {
            cmd.arg("-i")
                .arg(video_path)
                .arg("-ss")
                .arg(format!("{ts_secs:.3}"));
        }

        cmd.arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg("scale=iw*sar:ih");

        if ext == "jpg" || ext == "jpeg" {
            cmd.arg("-pix_fmt").arg("yuvj420p").arg("-q:v").arg(quality_str);
        } else if ext == "webp" {
            cmd.arg("-c:v").arg("libwebp").arg("-quality").arg("85");
        }

        cmd.arg(shot_path);

        let out = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                debug!(error = %e, "Failed executing ffmpeg command");
                return false;
            }
        };

        if out.status.success() && shot_path.exists() {
            if let Ok(meta) = fs::metadata(shot_path) {
                if meta.len() > 100 {
                    return true;
                }
            }
        }

        let _ = fs::remove_file(shot_path);
        false
    }

    async fn probe_video_duration(&self, video_path: &Path) -> Result<f64> {
        // 1. Try mediainfo video duration (most reliable for video streams inside Matroska/MP4)
        if let Ok(secs) = self.probe_mediainfo_video_duration(video_path).await {
            if secs > 0.1 {
                debug!(video_duration = secs, "Probed video duration via mediainfo");
                return Ok(secs);
            }
        }

        // 2. Try ffprobe stream=duration for video stream v:0
        let stream_dur = self.probe_ffprobe_stream_duration(video_path).await.ok();

        // 3. Try ffprobe format=duration
        let format_dur = self.probe_ffprobe_format_duration(video_path).await.ok();

        match (stream_dur, format_dur) {
            (Some(s), Some(f)) if s > 0.1 && f > 0.1 => Ok(s.min(f)),
            (Some(s), _) if s > 0.1 => Ok(s),
            (_, Some(f)) if f > 0.1 => Ok(f),
            _ => anyhow::bail!("Could not determine video duration from mediainfo or ffprobe"),
        }
    }

    async fn probe_mediainfo_video_duration(&self, video_path: &Path) -> Result<f64> {
        let out = Command::new(&self.config.packager.mediainfo_bin)
            .arg("--Inform=Video;%Duration%")
            .arg(video_path)
            .output()
            .await?;

        if !out.status.success() {
            anyhow::bail!("mediainfo duration exited with {}", out.status);
        }

        let txt = String::from_utf8_lossy(&out.stdout);
        let first = txt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let ms: f64 = first.parse().context("Failed to parse mediainfo duration ms")?;
        Ok(ms / 1000.0)
    }

    async fn probe_ffprobe_stream_duration(&self, video_path: &Path) -> Result<f64> {
        let out = Command::new(&self.config.packager.ffprobe_bin)
            .arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-show_entries")
            .arg("stream=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(video_path)
            .output()
            .await?;

        if !out.status.success() {
            anyhow::bail!("ffprobe stream duration exited with {}", out.status);
        }

        let txt = String::from_utf8_lossy(&out.stdout);
        let first = txt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let secs: f64 = first.parse().context("Failed to parse ffprobe stream duration")?;
        Ok(secs)
    }

    async fn probe_ffprobe_format_duration(&self, video_path: &Path) -> Result<f64> {
        let out = Command::new(&self.config.packager.ffprobe_bin)
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(video_path)
            .output()
            .await?;

        if !out.status.success() {
            anyhow::bail!("ffprobe format duration exited with {}", out.status);
        }

        let txt = String::from_utf8_lossy(&out.stdout);
        let first = txt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let secs: f64 = first.parse().context("Failed to parse ffprobe format duration")?;
        Ok(secs)
    }
}

/// Replaces full local disk paths in `Complete name : F:\Downloads\...` with just the filename.
pub fn sanitize_mediainfo_output(raw: &str, filename_only: &str) -> String {
    let mut cleaned_lines = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Complete name") {
            if let Some((left, _)) = line.split_once(':') {
                cleaned_lines.push(format!("{left}: {filename_only}"));
                continue;
            }
        }
        cleaned_lines.push(line.to_string());
    }
    cleaned_lines.join("\n")
}

fn sanitize_staging_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '[' | ']' | '(' | ')' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() {
        "release".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_mediainfo_strips_windows_path() {
        let sample = "General\nUnique ID : 12345\nComplete name                            : F:\\Downloads\\animez-staging\\[SubsPlease] Frieren - 01 (1080p).mkv\nFormat : Matroska";
        let sanitized = sanitize_mediainfo_output(sample, "[SubsPlease] Frieren - 01 (1080p).mkv");
        assert!(!sanitized.contains("F:\\Downloads"));
        assert!(sanitized.contains("Complete name                            : [SubsPlease] Frieren - 01 (1080p).mkv"));
    }

    #[tokio::test]
    async fn test_extract_screenshots_fallback_and_completion() {
        let temp_dir = tempfile::tempdir().unwrap();
        let video_path = temp_dir.path().join("short_sample.mkv");
        let shots_dir = temp_dir.path().join("screenshots");
        std::fs::create_dir_all(&shots_dir).unwrap();

        // Create a 5-second test MKV video
        let created = Command::new("ffmpeg")
            .arg("-y")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("testsrc=duration=5:size=160x120:rate=10")
            .arg(&video_path)
            .output()
            .await;

        if let Ok(out) = created {
            if out.status.success() {
                let mut config = AppConfig::default();
                config.packager.screenshot_count = 6;
                let packager = Packager::new(config);
                let shots = packager.extract_screenshots(&video_path, &shots_dir).await;
                assert!(shots.is_ok(), "Screenshot extraction should succeed: {:?}", shots.err());
                let shots = shots.unwrap();
                assert_eq!(shots.len(), 6, "Should extract exactly 6 screenshots");
                for s in shots {
                    assert!(s.exists());
                    assert!(s.metadata().unwrap().len() > 0);
                }
            }
        }
    }
}
