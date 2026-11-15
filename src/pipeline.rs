use crate::animez_client::AnimezClient;
use crate::config::AppConfig;
use crate::db::Database;
use crate::dupe_checker::DupeChecker;
use crate::flaresolverr::FlareSolverrClient;
use crate::models::{DiscoveredRelease, PackageBundle, ParsedRelease, ReleaseRecord, ReleaseState};
use crate::packager::Packager;
use crate::parser::{parse_anime_release, JikanResolver};
use crate::prowlarr::ProwlarrClient;
use crate::qbittorrent::QbittorrentClient;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, error, info, warn};

pub struct Pipeline {
    config: AppConfig,
    db: Database,
    prowlarr: ProwlarrClient,
    jikan: JikanResolver,
    dupe_checker: DupeChecker,
    qbit: QbittorrentClient,
    packager: Packager,
    animez: AnimezClient,
    flaresolverr: FlareSolverrClient,
}

impl Pipeline {
    pub fn new(config: AppConfig) -> Result<Self> {
        let db = Database::open(config.db_path())?;
        let prowlarr = ProwlarrClient::new(config.clone())?;
        let jikan = JikanResolver::new(config.clone(), db.clone())?;
        let flaresolverr = FlareSolverrClient::new(config.clone())?;
        let animez = AnimezClient::new(config.clone(), flaresolverr.clone())?;
        let dupe_checker = DupeChecker::new(
            config.clone(),
            ProwlarrClient::new(config.clone())?,
            animez.clone(),
        );
        let qbit = QbittorrentClient::new(config.clone())?;
        let packager = Packager::new(config.clone());

        Ok(Self {
            config,
            db,
            prowlarr,
            jikan,
            dupe_checker,
            qbit,
            packager,
            animez,
            flaresolverr,
        })
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn animez(&self) -> &AnimezClient {
        &self.animez
    }

    pub fn packager(&self) -> &Packager {
        &self.packager
    }

    /// Runs the continuous daemon polling loop until interrupted (`Ctrl+C`).
    pub async fn run_daemon(&self, interval_override_secs: Option<u64>, dry_run: bool) -> Result<()> {
        let interval = Duration::from_secs(
            interval_override_secs
                .unwrap_or(self.config.general.poll_interval_secs)
                .max(15),
        );
        info!(
            interval_secs = interval.as_secs(),
            dry_run = dry_run || self.config.general.dry_run,
            "Starting zbrr continuous polling daemon"
        );

        loop {
            if let Err(e) = self.run_cycle(dry_run).await {
                error!(error = %e, "Pipeline cycle encountered top-level error");
            }

            info!(
                sleep_secs = interval.as_secs(),
                "Cycle complete; sleeping until next poll"
            );
            tokio::select! {
                _ = tokio::time::sleep(interval) => {},
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C; shutting down daemon gracefully");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Runs a single discovery + state machine advancement cycle.
    pub async fn run_cycle(&self, dry_run: bool) -> Result<()> {
        let effective_dry_run = dry_run || self.config.general.dry_run;
        info!(dry_run = effective_dry_run, "Starting pipeline poll cycle");

        // Stage 1: Poll Prowlarr and insert newly discovered items into SQLite
        match self.discover_from_prowlarr().await {
            Ok(inserted) => {
                info!(newly_discovered = inserted, "Discovery stage finished");
            }
            Err(e) => {
                warn!(error = %e, "Prowlarr discovery failed; continuing with existing queued releases in SQLite");
            }
        }

        // Stage 2..6: Advance actionable releases through the state machine
        let limit = self.config.general.max_per_cycle.max(1);
        let db_clone = self.db.clone();
        let actionable =
            tokio::task::spawn_blocking(move || db_clone.get_actionable_releases(limit)).await??;

        if actionable.is_empty() {
            info!("No actionable releases pending in SQLite state machine");
            return Ok(());
        }

        for record in actionable {
            let id = record.id;
            let title = record.raw_title.clone();
            if let Err(e) = self.advance_release(record, effective_dry_run).await {
                let err_msg = format!("{e:#}");
                error!(
                    release_id = id,
                    title = %title,
                    error = %err_msg,
                    "Release failed during pipeline execution; marking FAILED in SQLite"
                );
                let db_err = self.db.clone();
                let _ = tokio::task::spawn_blocking(move || db_err.mark_failed(id, &err_msg)).await;
            }
        }

        Ok(())
    }

    /// Advances a specific release by ID through the pipeline.
    pub async fn process_release_by_id(&self, id: i64, dry_run: bool) -> Result<()> {
        let effective_dry_run = dry_run || self.config.general.dry_run;
        let db_clone = self.db.clone();
        let record = tokio::task::spawn_blocking(move || db_clone.get_release_by_id(id))
            .await??
            .with_context(|| format!("No release found with ID {id}"))?;

        if let Err(e) = self.advance_release(record, effective_dry_run).await {
            let err_msg = format!("{e:#}");
            let db_err = self.db.clone();
            let _ = tokio::task::spawn_blocking(move || db_err.mark_failed(id, &err_msg)).await;
            return Err(e);
        }
        Ok(())
    }

    async fn discover_from_prowlarr(&self) -> Result<usize> {
        let candidates = self.prowlarr.poll_releases().await?;
        let mut inserted_count = 0usize;

        for item in candidates {
            let parsed = parse_anime_release(&item.title);

            // Check filter rules first
            if let Err(reason) = self.prowlarr.check_filters(&item, &parsed) {
                debug!(
                    title = %item.title,
                    reason = %reason,
                    "Skipping Prowlarr candidate due to filter rule"
                );
                continue;
            }

            let db_check = self.db.clone();
            let guid = item.guid.clone();
            let info_hash = item.info_hash.clone();
            let norm_key = parsed.normalized_key.clone();

            let is_dupe = tokio::task::spawn_blocking(move || {
                db_check.is_duplicate_in_db(&guid, info_hash.as_deref(), &norm_key)
            })
            .await??;

            if is_dupe {
                debug!(title = %item.title, "Candidate already tracked in SQLite; skipping");
                continue;
            }

            let db_ins = self.db.clone();
            let item_clone = item.clone();
            let parsed_clone = parsed.clone();
            let id = tokio::task::spawn_blocking(move || {
                db_ins.insert_discovered(&item_clone, &parsed_clone)
            })
            .await??;

            info!(
                id = id,
                title = %item.title,
                key = %parsed.normalized_key,
                "Registered new release in DISCOVERED state"
            );
            inserted_count += 1;
        }

        Ok(inserted_count)
    }

    /// Drives a single `ReleaseRecord` forward from its current state until `SEEDING` or `SKIPPED`.
    async fn advance_release(&self, mut record: ReleaseRecord, dry_run: bool) -> Result<()> {
        let id = record.id;
        let parsed = record.parsed()?;

        // ---------------------------------------------------------------------
        // Stage 2: DISCOVERED -> DUPE_CHECKED (or SKIPPED)
        // ---------------------------------------------------------------------
        if record.state == ReleaseState::Discovered {
            info!(
                id = id,
                title = %record.raw_title,
                "Stage 2: Resolving MAL metadata & checking duplicates on animez.to"
            );

            // 1. Verify AnimeZ upload rule compliance (container, codecs, resolution, leaks)
            if let Err(rule_err) = crate::parser::check_animez_rule_compliance(&parsed) {
                info!(
                    id = id,
                    title = %record.raw_title,
                    reason = %rule_err,
                    "Release violates AnimeZ upload rules; marking SKIPPED"
                );
                let db_skip = self.db.clone();
                tokio::task::spawn_blocking(move || db_skip.mark_skipped(id, &rule_err)).await??;
                return Ok(());
            }

            let mal_meta = self.jikan.resolve_mal(&parsed.clean_title).await?;
            let mal_id = mal_meta.as_ref().map(|m| m.mal_id).or(record.mal_id);

            // 2. Check Ongoing Season Catch-Up Rule (AnimeZ Upload Rules Section 3)
            if let Some(catchup_reason) = self
                .dupe_checker
                .check_ongoing_catchup_rule(&parsed, mal_id, &self.db)
                .await?
            {
                info!(
                    id = id,
                    title = %record.raw_title,
                    reason = %catchup_reason,
                    "Ongoing episode N > 1 requires catch-up; searching Prowlarr for missing prior episodes..."
                );

                let queued_ids = match self
                    .search_and_queue_missing_prior_episodes(&parsed, mal_id)
                    .await
                {
                    Ok(ids) => ids,
                    Err(e) => {
                        warn!(
                            error = %e,
                            title = %record.raw_title,
                            "Failed searching Prowlarr for missing prior episodes"
                        );
                        Vec::new()
                    }
                };

                let skip_reason = if !queued_ids.is_empty() {
                    format!(
                        "Ongoing catch-up queued: Discovered and queued {} missing prior episode(s)/batch from Prowlarr; Episode {:?} will automatically unskip once sequence is caught up",
                        queued_ids.len(),
                        parsed.episode
                    )
                } else {
                    format!("{catchup_reason} (Prowlarr search found no prior episodes or batches)")
                };

                let db_skip = self.db.clone();
                tokio::task::spawn_blocking(move || db_skip.mark_skipped(id, &skip_reason)).await??;

                // Immediately find and advance the earliest actionable release (Episode 01 or batch) to download
                let db_find = self.db.clone();
                let clean_title = parsed.clean_title.clone();
                let earliest_rec = tokio::task::spawn_blocking(move || {
                    db_find.find_earliest_actionable_release_for_anime(&clean_title, mal_id)
                })
                .await??;

                if let Some(starter_rec) = earliest_rec {
                    let starter_id = starter_rec.id;
                    let starter_title = starter_rec.raw_title.clone();
                    let starter_parsed = starter_rec.parsed().ok();
                    let is_starter = starter_parsed.as_ref().is_some_and(|p| {
                        p.episode.is_some_and(|ep| ep <= 1) || p.is_season_pack
                    });

                    if is_starter {
                        info!(
                            starter_id = starter_id,
                            parent_id = id,
                            title = %starter_title,
                            "Immediately advancing missing catch-up release (Episode 01/batch) to download"
                        );
                        if let Err(e) = Box::pin(self.advance_release(starter_rec, dry_run)).await {
                            let err_msg = format!("{e:#}");
                            warn!(
                                starter_id = starter_id,
                                title = %starter_title,
                                error = %err_msg,
                                "Failed advancing catch-up release; marking FAILED in SQLite"
                            );
                            let db_err = self.db.clone();
                            let _ = tokio::task::spawn_blocking(move || db_err.mark_failed(starter_id, &err_msg)).await;
                        }
                    } else {
                        info!(
                            earliest_id = starter_id,
                            title = %starter_title,
                            "Queued catch-up releases require prior episodes; waiting for earlier sequence"
                        );
                    }
                }

                return Ok(());
            }

            // 3. Remote duplicate check on animez.to
            let dupe = self
                .dupe_checker
                .check_remote_duplicate(&parsed, mal_id)
                .await?;

            if dupe.is_dupe {
                info!(
                    id = id,
                    title = %record.raw_title,
                    reason = %dupe.reason,
                    "Release already exists on animez.to; marking SKIPPED"
                );
                let db_skip = self.db.clone();
                let reason = dupe.reason;
                tokio::task::spawn_blocking(move || db_skip.mark_skipped(id, &reason)).await??;
                return Ok(());
            }

            let db_upd = self.db.clone();
            tokio::task::spawn_blocking(move || db_upd.set_dupe_checked(id, mal_id)).await??;
            record.state = ReleaseState::DupeChecked;
            record.mal_id = mal_id;
        }

        // ---------------------------------------------------------------------
        // Stage 3a: DUPE_CHECKED -> DOWNLOADING
        // ---------------------------------------------------------------------
        if record.state == ReleaseState::DupeChecked {
            info!(
                id = id,
                title = %record.raw_title,
                "Stage 3: Adding release to qBittorrent under staging category"
            );
            let disc = DiscoveredRelease {
                guid: record.guid.clone(),
                title: record.raw_title.clone(),
                indexer_id: record.indexer_id,
                indexer_name: record.indexer_name.clone(),
                download_url: record.download_url.clone(),
                magnet_url: record.magnet_url.clone(),
                info_hash: record.source_info_hash.clone(),
                size_bytes: record.size_bytes,
                seeders: record.seeders,
                leechers: 0,
                publish_date: None,
            };

            let hash = self.qbit.add_source_download(&disc).await?;
            let db_dl = self.db.clone();
            let hash_clone = hash.clone();
            tokio::task::spawn_blocking(move || db_dl.set_downloading(id, &hash_clone)).await??;

            record.state = ReleaseState::Downloading;
            record.source_info_hash = Some(hash);
        }

        // ---------------------------------------------------------------------
        // Stage 3b: DOWNLOADING -> PACKAGING
        // ---------------------------------------------------------------------
        if record.state == ReleaseState::Downloading {
            let info_hash = record
                .source_info_hash
                .as_deref()
                .context("Release in DOWNLOADING state is missing source_info_hash")?;

            info!(
                id = id,
                title = %record.raw_title,
                hash = info_hash,
                "Polling qBittorrent for download completion"
            );
            let qbit_info = self.qbit.wait_for_completion(info_hash).await?;

            let raw_content_path = if !qbit_info.content_path.trim().is_empty() {
                qbit_info.content_path.clone()
            } else {
                PathBuf::from(&qbit_info.save_path)
                    .join(&qbit_info.name)
                    .to_string_lossy()
                    .to_string()
            };

            let local_content_path = self
                .config
                .map_qbit_path_to_local(&raw_content_path)
                .to_string_lossy()
                .to_string();

            let db_done = self.db.clone();
            let save_path_clone = qbit_info.save_path.clone();
            let content_path_clone = local_content_path.clone();
            tokio::task::spawn_blocking(move || {
                db_done.set_download_completed(id, &save_path_clone, &content_path_clone)
            })
            .await??;

            record.state = ReleaseState::Packaging;
            record.save_path = Some(qbit_info.save_path);
            record.content_path = Some(local_content_path);
        }

        // ---------------------------------------------------------------------
        // Stage 4: PACKAGING -> UPLOADING
        // ---------------------------------------------------------------------
        let mut bundle_opt: Option<PackageBundle> = None;
        if record.state == ReleaseState::Packaging {
            let content_str = record
                .content_path
                .as_deref()
                .context("Release in PACKAGING state is missing content_path")?;
            let content_path = PathBuf::from(content_str);

            info!(
                id = id,
                title = %record.raw_title,
                content_path = %content_path.display(),
                "Stage 4: Packaging media (.torrent, MediaInfo, screenshots)"
            );
            let bundle = self
                .packager
                .package_release(&content_path, None, false)
                .await?;

            let db_pkg = self.db.clone();
            let bundle_clone = bundle.clone();
            tokio::task::spawn_blocking(move || db_pkg.set_packaged(id, &bundle_clone)).await??;

            record.state = ReleaseState::Uploading;
            record.packaged_torrent_path =
                Some(bundle.torrent_path.to_string_lossy().to_string());
            record.mediainfo_path = Some(bundle.mediainfo_path.to_string_lossy().to_string());
            bundle_opt = Some(bundle);
        }

        // ---------------------------------------------------------------------
        // Stage 5 & 6: UPLOADING -> SEEDING
        // ---------------------------------------------------------------------
        if record.state == ReleaseState::Uploading {
            let bundle = match bundle_opt {
                Some(b) => b,
                None => self.reconstruct_bundle_from_record(&record)?,
            };

            let mal_meta = self.jikan.resolve_mal(&parsed.clean_title).await?;

            // Pre-upload duplicate check right before sending the upload POST
            if !dry_run {
                let recheck = self
                    .dupe_checker
                    .check_remote_duplicate(&parsed, record.mal_id)
                    .await?;
                if recheck.is_dupe {
                    // Check if this release was already uploaded to AnimeZ (e.g. prior attempt succeeded uploading, but failed tracker download/injection)
                    let mut reseed_rescued = false;
                    if let Some(matched_url) = recheck.matched_torrent_url.as_deref() {
                        if let Some(tid) = AnimezClient::extract_torrent_id_from_url(matched_url) {
                            let base = self.config.animez.base_url.trim_end_matches('/');
                            let dl_url = format!("{base}/torrents/{tid}/download");
                            let dest_path = bundle.torrent_path.with_extension("animez.torrent");
                            info!(
                                id = id,
                                torrent_id = tid,
                                download_url = %dl_url,
                                "Pre-upload dupe check found existing release on AnimeZ; attempting auto-reseed rescue"
                            );
                            if let Ok(()) = self.animez.download_tracker_torrent(&dl_url, &dest_path).await {
                                if let Some(save_path) = record.save_path.as_deref() {
                                    if let Ok(()) = self.qbit.inject_seeding_torrent(&dest_path, save_path).await {
                                        // Delete staging torrent if different
                                        if let Some(source_hash) = record.source_info_hash.as_deref() {
                                            let is_different = std::fs::read(&dest_path).ok().and_then(|b| {
                                                crate::qbittorrent::compute_infohash_from_torrent_bytes(&b)
                                            }).is_none_or(|h| !h.eq_ignore_ascii_case(source_hash));
                                            if is_different {
                                                let _ = self.qbit.delete_torrent(source_hash).await;
                                            }
                                        }

                                        let db_seed = self.db.clone();
                                        let page_url = matched_url.to_string();
                                        let _ = tokio::task::spawn_blocking(move || db_seed.set_seeding(id, Some(&page_url))).await;

                                        let db_unskip = self.db.clone();
                                        let clean_title = parsed.clean_title.clone();
                                        let _ = tokio::task::spawn_blocking(move || db_unskip.unskip_catchup_releases_for_anime(&clean_title, record.mal_id)).await;

                                        info!(
                                            id = id,
                                            title = %record.raw_title,
                                            page_url = %matched_url,
                                            "Successfully rescued and seeded release discovered during pre-upload dupe check!"
                                        );
                                        reseed_rescued = true;
                                    }
                                }
                            }
                        }
                    }

                    if reseed_rescued {
                        return Ok(());
                    }

                    info!(
                        id = id,
                        title = %record.raw_title,
                        reason = %recheck.reason,
                        "Pre-upload check detected duplicate uploaded while downloading; marking SKIPPED"
                    );
                    let db_skip = self.db.clone();
                    let reason = recheck.reason;
                    tokio::task::spawn_blocking(move || db_skip.mark_skipped(id, &reason))
                        .await??;
                    return Ok(());
                }
            }

            info!(
                id = id,
                title = %record.raw_title,
                dry_run = dry_run,
                "Stage 5: Uploading packaged release to animez.to"
            );
            let upload_res = self
                .animez
                .upload_release(&parsed, mal_meta.as_ref(), &bundle, dry_run)
                .await?;

            if !upload_res.dry_run {
                let save_path = record
                    .save_path
                    .as_deref()
                    .context("Missing save_path for auto-reseed injection")?;
                let seed_torrent_path = match upload_res.downloaded_tracker_torrent_path.as_deref() {
                    Some(p) => p,
                    None => {
                        anyhow::bail!(
                            "Release id={} was uploaded but failed to download official re-signed tracker .torrent; aborting re-seed injection",
                            id
                        );
                    }
                };

                info!(
                    id = id,
                    torrent = %seed_torrent_path.display(),
                    save_path = save_path,
                    "Stage 6: Injecting tracker torrent into qBittorrent for immediate seeding"
                );
                self.qbit
                    .inject_seeding_torrent(seed_torrent_path, save_path)
                    .await?;

                // Delete the original staging torrent from qBittorrent (keep downloaded media files intact on disk)
                if let Some(source_hash) = record.source_info_hash.as_deref() {
                    let is_different = upload_res
                        .downloaded_tracker_torrent_path
                        .as_ref()
                        .map(|p| {
                            std::fs::read(p)
                                .ok()
                                .and_then(|b| crate::qbittorrent::compute_infohash_from_torrent_bytes(&b))
                                .is_none_or(|h| !h.eq_ignore_ascii_case(source_hash))
                        })
                        .unwrap_or(true);

                    if is_different {
                        info!(
                            id = id,
                            hash = source_hash,
                            "Deleting original staging torrent from qBittorrent (keeping media files intact)"
                        );
                        if let Err(e) = self.qbit.delete_torrent(source_hash).await {
                            warn!(
                                id = id,
                                hash = source_hash,
                                error = %e,
                                "Failed to delete original staging torrent from qBittorrent"
                            );
                        }
                    }
                }
            }

            let db_seed = self.db.clone();
            let page_url = upload_res.torrent_page_url.clone();
            tokio::task::spawn_blocking(move || db_seed.set_seeding(id, page_url.as_deref()))
                .await??;

            info!(
                id = id,
                title = %record.raw_title,
                dry_run = upload_res.dry_run,
                "Release successfully transitioned to SEEDING state"
            );

            // Automatically un-skip any subsequent episodes for this anime that were waiting for catch-up
            let db_unskip = self.db.clone();
            let clean_title = parsed.clean_title.clone();
            let unskipped = tokio::task::spawn_blocking(move || {
                db_unskip.unskip_catchup_releases_for_anime(&clean_title, record.mal_id)
            })
            .await??;

            if unskipped > 0 {
                info!(
                    anime = %parsed.clean_title,
                    unskipped = unskipped,
                    "Un-skipped subsequent episode(s) now that required upload sequence is established"
                );
            }
        }

        Ok(())
    }

    /// Searches Prowlarr for missing prior episodes or batches for an ongoing anime series
    /// when ZBRR_ANIMEZ_ENFORCE_ONGOING_CATCHUP is enabled, and queues them in DISCOVERED state.
    async fn search_and_queue_missing_prior_episodes(
        &self,
        parsed: &ParsedRelease,
        _mal_id: Option<u64>,
    ) -> Result<Vec<i64>> {
        let target_ep = match parsed.episode {
            Some(ep) if ep > 1 => ep,
            _ => return Ok(Vec::new()),
        };

        let raw_candidates = self.prowlarr.search_missing_prior_episodes(parsed).await?;
        if raw_candidates.is_empty() {
            return Ok(Vec::new());
        }

        let target_norm = crate::dupe_checker::normalize_alnum(&parsed.clean_title);
        let target_season = parsed.season.unwrap_or(1);
        let target_res = parsed.resolution.as_deref().unwrap_or("1080p");

        let mut batch_candidates: Vec<(DiscoveredRelease, ParsedRelease)> = Vec::new();
        let mut episode_candidates: std::collections::HashMap<u32, (DiscoveredRelease, ParsedRelease)> =
            std::collections::HashMap::new();

        for item in raw_candidates {
            let cand_parsed = parse_anime_release(&item.title);

            // 1. Check AnimeZ upload rules compliance
            if crate::parser::check_animez_rule_compliance(&cand_parsed).is_err() {
                continue;
            }

            // 2. Check Prowlarr filters (e.g. min seeders, link validity)
            if self.prowlarr.check_filters(&item, &cand_parsed).is_err() {
                continue;
            }

            // 3. Verify clean title matches
            let cand_norm = crate::dupe_checker::normalize_alnum(&cand_parsed.clean_title);
            if cand_norm.is_empty()
                || (cand_norm != target_norm
                    && !cand_norm.contains(&target_norm)
                    && !target_norm.contains(&cand_norm))
            {
                continue;
            }

            // 4. Verify season matches
            let cand_season = cand_parsed.season.unwrap_or(1);
            if cand_season != target_season {
                continue;
            }

            // 5. Match release group if current release has one
            if let Some(ref target_grp) = parsed.release_group {
                if let Some(ref cand_grp) = cand_parsed.release_group {
                    if !cand_grp.eq_ignore_ascii_case(target_grp) {
                        continue;
                    }
                } else {
                    continue;
                }
            }

            // 6. Match resolution if available
            if let Some(ref cand_res) = cand_parsed.resolution {
                if !cand_res.eq_ignore_ascii_case(target_res) {
                    continue;
                }
            }

            // 7. Check if already tracked in SQLite
            let db_check = self.db.clone();
            let guid = item.guid.clone();
            let info_hash = item.info_hash.clone();
            let norm_key = cand_parsed.normalized_key.clone();
            let is_dupe = tokio::task::spawn_blocking(move || {
                db_check.is_duplicate_in_db(&guid, info_hash.as_deref(), &norm_key)
            })
            .await??;

            if is_dupe {
                continue;
            }

            // 8. Classify as batch or prior single episode
            if cand_parsed.is_season_pack {
                batch_candidates.push((item, cand_parsed));
            } else if let Some(cand_ep) = cand_parsed.episode {
                if cand_ep < target_ep && cand_ep >= 1 {
                    // Keep candidate with highest seeders for this episode
                    match episode_candidates.get(&cand_ep) {
                        Some((existing_item, _)) if existing_item.seeders >= item.seeders => {}
                        _ => {
                            episode_candidates.insert(cand_ep, (item, cand_parsed));
                        }
                    }
                }
            }
        }

        let mut to_queue: Vec<(DiscoveredRelease, ParsedRelease)> = Vec::new();

        // If a batch candidate exists, prioritize it
        if let Some((best_batch_item, best_batch_parsed)) =
            batch_candidates.into_iter().max_by_key(|(i, _)| i.seeders)
        {
            to_queue.push((best_batch_item, best_batch_parsed));
        } else {
            // Sort single episodes in ascending sequence order: 1, 2, ... target_ep - 1
            let mut sorted_eps: Vec<_> = episode_candidates.into_iter().collect();
            sorted_eps.sort_by_key(|(ep, _)| *ep);
            for (_, pair) in sorted_eps {
                to_queue.push(pair);
            }
        }

        let mut queued_ids = Vec::new();
        for (item, cand_parsed) in to_queue {
            let db_ins = self.db.clone();
            let item_clone = item.clone();
            let cand_clone = cand_parsed.clone();

            let new_id = tokio::task::spawn_blocking(move || {
                db_ins.insert_discovered(&item_clone, &cand_clone)
            })
            .await??;

            info!(
                id = new_id,
                title = %item.title,
                ep = ?cand_parsed.episode,
                is_batch = cand_parsed.is_season_pack,
                "Queued missing prior catch-up release in DISCOVERED state"
            );
            queued_ids.push(new_id);
        }

        Ok(queued_ids)
    }

    fn reconstruct_bundle_from_record(&self, record: &ReleaseRecord) -> Result<PackageBundle> {
        let torrent_path = PathBuf::from(
            record
                .packaged_torrent_path
                .as_deref()
                .context("Missing packaged_torrent_path on record")?,
        );
        let mediainfo_path = PathBuf::from(
            record
                .mediainfo_path
                .as_deref()
                .context("Missing mediainfo_path on record")?,
        );
        let mediainfo_text = fs::read_to_string(&mediainfo_path)?;
        let screenshots: Vec<PathBuf> = record
            .screenshots_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let content_path = PathBuf::from(record.content_path.as_deref().unwrap_or_default());
        let primary_video_path =
            Packager::find_primary_video_file(&content_path).unwrap_or_else(|_| content_path.clone());

        Ok(PackageBundle {
            torrent_path,
            torrent_info_hash: record.tracker_info_hash.clone().unwrap_or_default(),
            mediainfo_path,
            mediainfo_text,
            screenshots,
            content_path,
            primary_video_path,
            total_size_bytes: record.size_bytes,
            file_count: 1,
            piece_length: 0,
        })
    }

    /// Runs `zbrr doctor` across external CLI binaries, SQLite, Prowlarr, qBittorrent, FlareSolverr, and AnimeZ.
    pub async fn test_all_connections(&self) -> Result<()> {
        println!("=== AnimeZ (zbrr) Connectivity & Dependency Check ===");

        // 1. External CLI tools (ffmpeg, ffprobe, mediainfo)
        println!("\n[1/5] External Media Packaging Binaries:");
        for (name, res) in self.packager.verify_binaries().await {
            match res {
                Ok(ver) => println!("  [OK]   {name}: {ver}"),
                Err(e) => println!("  [WARN] {name}: {e}"),
            }
        }

        // 2. SQLite State Database
        println!("\n[2/5] SQLite State Database:");
        match self.db.list_releases(None, 1) {
            Ok(_) => println!("  [OK]   SQLite WAL database initialized at {}", self.config.db_path().display()),
            Err(e) => println!("  [FAIL] SQLite database error: {e}"),
        }

        // 3. Prowlarr
        println!("\n[3/5] Prowlarr Server ({}):", self.config.prowlarr.url);
        match self.prowlarr.test_connection().await {
            Ok(msg) => println!("  [OK]   {msg}"),
            Err(e) => println!("  [WARN] {e}"),
        }

        // 4. qBittorrent
        println!("\n[4/5] qBittorrent WebUI ({}):", self.config.qbittorrent.url);
        match self.qbit.test_connection().await {
            Ok(msg) => println!("  [OK]   {msg}"),
            Err(e) => println!("  [WARN] {e}"),
        }

        // 5. FlareSolverr & AnimeZ.to
        println!("\n[5/5] FlareSolverr ({}) & Tracker ({}):", self.config.flaresolverr.url, self.config.animez.base_url);
        match self.flaresolverr.test_connection().await {
            Ok(msg) => println!("  [OK]   {msg}"),
            Err(e) => println!("  [WARN] FlareSolverr: {e}"),
        }

        if self.flaresolverr.has_session_cookies().await || self.animez.has_credentials() {
            match self.animez.test_connection().await {
                Ok(msg) => println!("  [OK]   AnimeZ.to: {msg}"),
                Err(e) => println!("  [WARN] AnimeZ.to: {e}"),
            }
        } else {
            println!("  [INFO] No AnimeZ credentials or cached cookies in {} yet (configure ZBRR_ANIMEZ_USERNAME, ZBRR_ANIMEZ_PASSWORD, and ZBRR_ANIMEZ_PID in .env).", self.config.flaresolverr.cookies_file.display());
        }

        Ok(())
    }
}
