use anyhow::{Context, Result};
use chrono::Utc;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Entry inside `info.files` for multi-file torrents.
/// Keys MUST be declared in alphabetical order: `length`, `path`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TorrentFileEntry {
    pub length: u64,
    pub path: Vec<String>,
}

/// BitTorrent `info` dictionary.
/// CRITICAL: `serde_bencode` serializes struct fields in declaration order.
/// Fields MUST remain in strict lexicographical order by serialized key name:
/// 1. `files`
/// 2. `length`
/// 3. `name`
/// 4. `piece length`
/// 5. `pieces`
/// 6. `private`
/// 7. `source`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<TorrentFileEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    pub name: String,
    #[serde(rename = "piece length")]
    pub piece_length: u64,
    pub pieces: ByteBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Root `.torrent` dictionary.
/// Keys MUST be declared in strict lexicographical order:
/// 1. `announce`
/// 2. `created by`
/// 3. `creation date`
/// 4. `info`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentRoot {
    pub announce: String,
    #[serde(rename = "created by", skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(rename = "creation date", skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<i64>,
    pub info: TorrentInfo,
}

#[derive(Debug, Clone)]
pub struct TorrentBuildOutput {
    pub torrent_path: PathBuf,
    pub info_hash: String,
    pub total_size_bytes: u64,
    pub piece_length: u64,
    pub piece_count: usize,
    pub file_count: usize,
}

#[derive(Debug, Clone)]
struct DiskFileSpan {
    abs_path: PathBuf,
    rel_components: Vec<String>,
    length: u64,
    stream_offset: u64,
}

/// Selects an automatic power-of-two piece size between 256 KiB and 16 MiB based on total payload size.
pub fn select_piece_length(total_size: u64) -> u64 {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if total_size < 256 * MIB {
        256 * KIB
    } else if total_size < 512 * MIB {
        512 * KIB
    } else if total_size < GIB {
        MIB
    } else if total_size < 2 * GIB {
        2 * MIB
    } else if total_size < 4 * GIB {
        4 * MIB
    } else if total_size < 8 * GIB {
        8 * MIB
    } else {
        16 * MIB
    }
}

/// Returns true if a filename is OS junk or incomplete download metadata that should be excluded.
pub fn is_ignored_os_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "thumbs.db" | ".ds_store" | "desktop.ini" | "ehthumbs.db"
    ) || lower.ends_with(".!qb")
        || lower.ends_with(".part")
}

/// Builds a private `.torrent` file (`private=1`, `source="animez.to"`) from a local file or directory,
/// hashing pieces in parallel across CPU cores via `rayon`.
pub fn build_private_torrent(
    content_path: &Path,
    output_torrent_path: &Path,
    announce_url: &str,
    source_tag: &str,
) -> Result<TorrentBuildOutput> {
    if !content_path.exists() {
        anyhow::bail!(
            "Cannot build torrent: content path does not exist: {}",
            content_path.display()
        );
    }

    let name = content_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("Content path has no valid UTF-8 file/folder name")?
        .to_string();

    let is_dir = content_path.is_dir();
    let mut spans = Vec::new();
    let mut total_size: u64 = 0;

    if is_dir {
        let mut discovered_files: Vec<(PathBuf, Vec<String>, u64)> = Vec::new();
        for entry in WalkDir::new(content_path).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if is_ignored_os_file(p) {
                continue;
            }
            let rel = p.strip_prefix(content_path)?;
            let components: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect();
            if components.is_empty() {
                continue;
            }
            let len = entry.metadata()?.len();
            discovered_files.push((p.to_path_buf(), components, len));
        }

        // Sort files deterministically by relative path components
        discovered_files.sort_by(|a, b| a.1.cmp(&b.1));

        if discovered_files.is_empty() {
            anyhow::bail!(
                "Directory '{}' contains no valid files to package into a torrent",
                content_path.display()
            );
        }

        for (abs_path, rel_components, len) in discovered_files {
            spans.push(DiskFileSpan {
                abs_path,
                rel_components,
                length: len,
                stream_offset: total_size,
            });
            total_size += len;
        }
    } else {
        let len = fs::metadata(content_path)?.len();
        spans.push(DiskFileSpan {
            abs_path: content_path.to_path_buf(),
            rel_components: vec![name.clone()],
            length: len,
            stream_offset: 0,
        });
        total_size = len;
    }

    let piece_length = select_piece_length(total_size);
    let piece_count = if total_size == 0 {
        0
    } else {
        total_size.div_ceil(piece_length) as usize
    };

    // Hash all pieces in parallel across CPU cores using rayon
    let piece_hashes: Result<Vec<[u8; 20]>> = (0..piece_count)
        .into_par_iter()
        .map(|piece_idx| {
            let piece_start = (piece_idx as u64) * piece_length;
            let piece_end = (piece_start + piece_length).min(total_size);
            let piece_size = (piece_end - piece_start) as usize;
            let buf = read_stream_range(&spans, piece_start, piece_size)?;
            let mut hasher = Sha1::new();
            hasher.update(&buf);
            let digest: [u8; 20] = hasher.finalize().into();
            Ok(digest)
        })
        .collect();

    let piece_hashes = piece_hashes?;
    let mut pieces_concat = Vec::with_capacity(piece_hashes.len() * 20);
    for h in &piece_hashes {
        pieces_concat.extend_from_slice(h);
    }

    let info = if is_dir {
        let files_list = spans
            .iter()
            .map(|s| TorrentFileEntry {
                length: s.length,
                path: s.rel_components.clone(),
            })
            .collect();
        TorrentInfo {
            files: Some(files_list),
            length: None,
            name,
            piece_length,
            pieces: ByteBuf::from(pieces_concat),
            private: Some(1),
            source: Some(source_tag.to_string()),
        }
    } else {
        TorrentInfo {
            files: None,
            length: Some(total_size),
            name,
            piece_length,
            pieces: ByteBuf::from(pieces_concat),
            private: Some(1),
            source: Some(source_tag.to_string()),
        }
    };

    // Compute canonical SHA-1 info_hash from the bencoded `info` dictionary
    let info_bencoded =
        serde_bencode::to_bytes(&info).context("Failed to bencode TorrentInfo dictionary")?;
    let mut info_hasher = Sha1::new();
    info_hasher.update(&info_bencoded);
    let info_hash = hex::encode(info_hasher.finalize());

    let root = TorrentRoot {
        announce: announce_url.to_string(),
        created_by: Some("zbrr/0.1.0".to_string()),
        creation_date: Some(Utc::now().timestamp()),
        info,
    };

    let torrent_bytes =
        serde_bencode::to_bytes(&root).context("Failed to bencode TorrentRoot dictionary")?;

    if let Some(parent) = output_torrent_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(output_torrent_path, &torrent_bytes).with_context(|| {
        format!(
            "Failed to write .torrent file to {}",
            output_torrent_path.display()
        )
    })?;

    Ok(TorrentBuildOutput {
        torrent_path: output_torrent_path.to_path_buf(),
        info_hash,
        total_size_bytes: total_size,
        piece_length,
        piece_count,
        file_count: spans.len(),
    })
}

/// Reads a contiguous byte slice `[offset .. offset + len)` across one or more concatenated files.
fn read_stream_range(spans: &[DiskFileSpan], offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    let mut written = 0usize;
    let mut cur_stream_pos = offset;
    let end_stream_pos = offset + (len as u64);

    // Binary search for the first file containing `cur_stream_pos`
    let start_idx = spans
        .partition_point(|s| s.stream_offset + s.length <= cur_stream_pos);

    for span in &spans[start_idx..] {
        if cur_stream_pos >= end_stream_pos {
            break;
        }
        if span.length == 0 {
            continue;
        }
        let file_start_offset = cur_stream_pos.saturating_sub(span.stream_offset);
        let available_in_file = span.length.saturating_sub(file_start_offset);
        let remaining_to_read = (len - written) as u64;
        let to_read = available_in_file.min(remaining_to_read) as usize;

        if to_read > 0 {
            let mut f = File::open(&span.abs_path).with_context(|| {
                format!(
                    "Failed to open payload file for hashing: {}",
                    span.abs_path.display()
                )
            })?;
            f.seek(SeekFrom::Start(file_start_offset))?;
            f.read_exact(&mut out[written..written + to_read])?;
            written += to_read;
            cur_stream_pos += to_read as u64;
        }
    }

    if written != len {
        anyhow::bail!("Unexpected short read while hashing piece: expected {len}, read {written}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_single_file_private_torrent_and_key_order() -> Result<()> {
        let dir = tempdir()?;
        let video_path = dir.path().join("[SubsPlease] Test - 01 (1080p).mkv");
        let payload = vec![0x42u8; 600_000];
        fs::write(&video_path, &payload)?;

        let torrent_out = dir.path().join("out.torrent");
        let built = build_private_torrent(
            &video_path,
            &torrent_out,
            "https://tracker.animez.to/secret/announce",
            "animez.to",
        )?;

        assert_eq!(built.total_size_bytes, 600_000);
        assert_eq!(built.piece_length, 262_144);
        assert_eq!(built.piece_count, 3);
        assert_eq!(built.file_count, 1);
        assert_eq!(built.info_hash.len(), 40);

        let raw_bytes = fs::read(&torrent_out)?;
        let decoded: TorrentRoot = serde_bencode::from_bytes(&raw_bytes)?;
        assert_eq!(
            decoded.announce,
            "https://tracker.animez.to/secret/announce"
        );
        assert_eq!(decoded.info.private, Some(1));
        assert_eq!(decoded.info.source.as_deref(), Some("animez.to"));
        assert_eq!(decoded.info.length, Some(600_000));
        assert!(decoded.info.files.is_none());
        Ok(())
    }

    #[test]
    fn test_multi_file_directory_torrent_and_junk_exclusion() -> Result<()> {
        let dir = tempdir()?;
        let pack_dir = dir.path().join("[Group] Anime Season 1 (1080p)");
        let sub_dir = pack_dir.join("Extras");
        fs::create_dir_all(&sub_dir)?;

        fs::write(pack_dir.join("Episode 01.mkv"), vec![1u8; 200_000])?;
        fs::write(pack_dir.join("Episode 02.mkv"), vec![2u8; 200_000])?;
        fs::write(sub_dir.join("NCOP.mkv"), vec![3u8; 100_000])?;
        // OS junk files that must be ignored
        fs::write(pack_dir.join("Thumbs.db"), b"junk")?;
        fs::write(pack_dir.join(".DS_Store"), b"junk")?;

        let torrent_out = dir.path().join("pack.torrent");
        let built = build_private_torrent(
            &pack_dir,
            &torrent_out,
            "https://tracker.animez.to/passkey/announce",
            "animez.to",
        )?;

        assert_eq!(built.file_count, 3);
        assert_eq!(built.total_size_bytes, 500_000);

        let raw_bytes = fs::read(&torrent_out)?;
        let decoded: TorrentRoot = serde_bencode::from_bytes(&raw_bytes)?;
        let files = decoded.info.files.unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, vec!["Episode 01.mkv".to_string()]);
        assert_eq!(files[1].path, vec!["Episode 02.mkv".to_string()]);
        assert_eq!(
            files[2].path,
            vec!["Extras".to_string(), "NCOP.mkv".to_string()]
        );
        Ok(())
    }
}
