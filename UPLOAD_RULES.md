# AnimeZ (animez.to) Upload Rules

> [!IMPORTANT]
> These are the official upload rules from [animez.to/rules/upload-rules](https://animez.to/rules/upload-rules).
> All pipeline operations, packagers, and dupe-checkers in `zbrr` must strictly adhere to these invariants.

---

## Key Automation Takeaways for `zbrr`

1. **Screenshots (Minimum 6 for Anime)**:
   - Anime uploads require a **minimum of 6 screenshots** (maximum 9).
   - Screenshots must match video resolution, have no black bars/cropping, and be under 20MB.
   - Default `packager.screenshot_count` must be set to **6**.
2. **Ongoing Series & Catch-Up Batches**:
   - If an ongoing anime already has several episodes released when starting to upload, you must upload a **batch of all available episodes (e.g. Ep 01–07)** rather than a standalone episode 07.
   - Once caught up, new episodes may be uploaded weekly as single episodes in sequential order.
3. **Single File Uploads & Directory Structure**:
   - Single file torrents must **NOT** be placed inside a folder unless the folder carries important metadata not in the filename.
4. **No Padding Files or Junk**:
   - BitTorrent files must not contain padding files, `.txt`, `.nfo`, `.sfv`, `.ini`, or random image files.
5. **Full MediaInfo**:
   - Raw English MediaInfo must be included in full without stripping technical stream data.
6. **Unique Database Entry**:
   - Each upload must correspond to a single database entry (one AniDB / AniList / MAL ID per torrent). No multi-series or custom collection packs.

---

## 1. General Rules

- **Allowed Content**: Anime (TV, TV Short, Movie, Special, OVA, ONA, Music Video), Printed Media (Manga, Light Novels, One-Shots), and Audiobooks.
  - Must be indexed on **AniDB**, **MyAnimeList**, or **AniList**.
  - Printed media must be indexed on **MangaUpdates** or **MyAnimeList**.
- **Single Database Entry**: Each upload must correspond to a single database entry (one AniDB/AniList/MAL ID per torrent). No multi-series or custom collection packs. If a Blu-ray box set cannot be easily divided by disc along these lines, it may be uploaded under the auspice of the more significant AniDB ID.
- **No Mixed Content**: Content must not be mixed. If there are anime episodes and OVAs for a title, they must be uploaded as separate torrents. Specials may be included with the anime series if they are listed together under the same AniDB ID or separately if there is a MAL ID specifically for them.
- **No Pure Audio**: Pure audio uploads (MP3, FLAC, OST) are not allowed. Official music video collections (BDMV) and creditless OP/ED bundled with video are permitted under the Music Video format.
- **Production Standard**: Original audio and video content must be professionally produced, even if it is later encoded by a fansub group. Fan projects are not permitted. Fansubs are allowed, as are scanlations and doujinshi from recognized circles.
- **Seeding Commitment**: Do not upload torrents if you are not going to seed them! Make sure your torrents are well-seeded for at least **24 of the first 48 hours**, or until 3 additional seeders have been created.
- **Connectability**: When uploading a torrent you must be connectable (ports properly forwarded).
- **Bandwidth**: Do not upload more than your Internet connection can handle and seed at a decent rate.
- **No Junk / Padding Files**: Torrents must not include irrelevant files such as `.txt`, `.ini`, `.sfv`, `.nfo`, random wallpapers, or covers that aren't included on the Blu-ray. No archived files (`.zip`, `.rar`, etc.) for anime uploads. Exceptions: full Blu-ray/DVD discs may contain additional material (booklets, artbooks, creditless OP/ED, etc.).
- **Padding Files**: The `.torrent` file should contain **no padding files of any kind**.
- **Do Not Rename Files**: Filenames within the torrent must not be changed. Leave the original release group name intact. Removing the release group tag will be penalized.
- **Single File Folders**: Single file uploads should **NOT** be uploaded inside folders unless the folder name carries important information that the file name does not (e.g. group name).
- **Respect Exclusives**: Respect other sites' exclusives. Get permission first.
- **Adult Content**: Adult/Hentai content is allowed only when the torrent is flagged as adult content.
- **Protection of Minors**: No screenshots or cover images depicting characters who appear underage in sexual activity, regardless of stated fictional age.
- **No Pre-Air Leaks**: Pre-air leaks of anime are not permitted.
- **No Web Stream Rips (YouTube etc.)**: Video ripped from YouTube and similar sites is not allowed.
- **Staff Authority**: Staff reserves the right to remove any file(s) they deem inappropriate. Their decision is final.

---

## 2. Anime Upload Specifications

### Containers
- **Allowed**: `MKV`, `MP4`, `AVI`
- **Exceptions**:
  - Full DVD Discs: `.VOB` and all other files that come with the DVD disc file structure.
  - Full BluRay Discs: `.M2TS` and all other files that come with the BluRay disc file structure.
  - `OGG`/`OGM` releases may be uploaded if no better version exists for that anime.
- **Not Allowed**: `ISO` files or any other containers not mentioned above.

### Video Codecs
- **Allowed**: `H264`/`x264`/`AVC`, `H265`/`x265`/`HEVC`. 8-bit or 10-bit color depth.
- **Exceptions**:
  - `MPEG2` for Full DVD discs and HDTV recordings.
  - `VC-1`/`MPEG2` for BluRay only if that's what is on the original disc.
- **Not Allowed**: Any codec not mentioned above.

### Audio Codecs
- **Allowed**: `MP3`, `AAC`, `HE-AAC`, `AC3` (Dolby Digital), `E-AC3`, `Dolby TrueHD`, `DTS`, `DTS-HD (MA)`, `FLAC`, `LPCM`/`WAV`, `OPUS`
- **Not Allowed**: Any codec not mentioned above.
- **Notes**:
  - AC3(DD) / E-AC3 (DDP) will not trump existing AAC uploads with the same audio bitrate and channels (when all else is equal and only audio codecs differ).
  - Uploads with TrueHD/Atmos audio **must have either an embedded or standalone AC-3 track** due to player compatibility.

### Rip Types
- **Allowed**:
  - **BluRay**: Raw (Full BluRay disc BD25/50), REMUX, BluRay/BDRip (4k, 1080p, 720p, SD encodes)
  - **DVD**: Raw (Full DVD disc DVD5/9), DVDRip
  - **Broadcast/Streaming**: HDTV, WEB-DL, WEBRip
- **Not Allowed**: Any rip type not mentioned above.

### Remux Requirements
- One original language track (Japanese); up to two in the case of surround/stereo.
- Commentary tracks, if any.
- One dub track per language (English dub encouraged for dual-audio releases).
- Exception: A REMUX with TrueHD/Atmos audio must include an embedded or standalone AC-3 track.

### Quality & Encoding Standards
- **Proper Encodes Only**: Any encodes where the size and/or bitrate imply bad quality will be deleted. Bad re-encodes of WEBRips are not allowed.
- **BluRay Audio**: Re-encoded audio is not allowed unless the source is lossless.
- **Resolution**: Minimum resolution of **480p**.
- **Audio Bitrate**: Minimum bitrate of **128 kbps** (smaller bitrate allowed for WEB-DL).
- **Watermarks**: Watermarks are prohibited (except official TV broadcast overlays or streaming service bugs).
- **Audio Language**: Original Japanese audio is preferred. English dubs and dual-audio releases are welcome. At least one track must be the original language.
- **No Upscaling / No Frame Interpolation**: Upscaled video and frame-interpolated video (e.g. 29.97/23.976 fps -> 59.94 fps) are strictly banned.
- **No Re-encoding**: Only videos encoded from the original source are permitted.
- **Do Not Rename Releases**: Never rename scene or fansub releases!
- **Raw Anime**: Subtitle-free RAW anime is allowed only if no subtitled version exists on site.
- **Subtitles**: External subtitles must be uploaded to the subtitle database, not packaged inside the torrent.
- **BD/DVD Extras**: Menus, art galleries, bloopers, commentary videos permitted with full BD/DVD and REMUX only (not with encodes).
- **No Rip Tool Garbage**: Do not include MakeMKV, AnyDVD, DVDFab, `.miniso` files in BDMV/DVD uploads.

---

## 3. Anime TV Series & Ongoing Seasons

### Episode Uploads & Season Batches
- **Single Episodes**: Single episodes are **only allowed while the series is still airing**.
- **Sequential Uploads**: Episodes must be uploaded in sequence (do not upload episode 2 before episode 1).
- **No Random Episodes**: Standalone random episodes are deleted.
- **Completed Seasons**: Completed series/seasons must be uploaded as a **complete batch/pack**.
- **Multi-Cour Series**: Each cour/season should be a separate upload.
- **Consistency**: Batch releases must have consistent fansub groups, resolutions, and containers.
- **Hardsubs vs Softsubs**: Hardsubs are only allowed if no softsub version is available on the site, or if the hardsub is significantly higher quality (e.g. HD hardsub vs SD softsub).
- **Coexistence**: Multiple torrents of a single anime can coexist if they vary in fansub group, resolution, video codec, audio language, audio codec, or bit depth.

### Ongoing Season Exclusivity & Catch-Up Rules
- **Catch-Up Batch Requirement**: If an anime already has several episodes out when you start uploading, you **must upload a batch of all available episodes to date** (e.g. Episodes 01–07). You cannot jump straight to uploading a standalone episode 07.
- **Group Exclusivity**: When a user begins uploading an ongoing season with a particular fansub group's release, that user acquires the right to upload that fansub's release for that series. Another user may not take over without consent.
  - *Exception*: Rights are revoked if the uploader fails to upload an episode within **3 weeks** of availability.
  - Rights expire **1 week after the final episode has aired** and the series is marked "Finished Airing" on AniDB/MAL.

### Long Ongoing Shows (One Piece, Detective Conan, etc.)
- Seasonal batches are preferred over massive monolithic torrents.
- Keep resolution consistent (avoid mixing SD and HD in one release).
- When a long series finishes, a complete batch is allowed and will not duplicate seasonal batches.

---

## 4. Anime Movies
- Custom collections are prohibited. Each upload must have its own AniList/MAL/AniDB entry.
- Box sets (multiple movies per disc) are only allowed in RAW form; otherwise upload individually.
- Only the movie video file itself belongs in the torrent (external subs go to the subtitle database).
- Extras/Specials must be uploaded separately if they don't share the same AniDB ID as the parent movie.
- Hardsubs are only allowed if no softsubs exist or if quality is strictly superior.

---

## 5. Printed Media (Manga, Light Novels, One-Shots)
- **Allowed Formats**: `JPG`/`PNG`, `CBZ`/`CBR`, `ZIP`/`RAR`, `EPUB`, `PDF`, `WEBP`, `TIF`/`TIFF`, `GIF`, `BMP`.
- **Entries**: Must correspond to MangaUpdates, MyAnimeList, or AniList entries.
- **No Format Mixtures**: Do not mix PDF and EPUB in the same torrent.
- **No Watermarked Scraper Rips**: Rips from MangaFox, MangaHere, MangaPanda, MangaReader are banned. Scanlation group watermarks are allowed.
- **Minimum Upload Unit**: Single chapters are prohibited. At least **one full volume** must be uploaded. For ongoing/dropped series without volume releases, batch all available chapters. Exception: One-Shots and single-chapter doujinshi.
- **Organization**: Organize into folders by volume/chapter. File renaming for organization is permitted for manga.

---

## 6. Descriptions, MediaInfo & Screenshots

### Descriptions
- Must be written in **English**.
- Credit the fansub or scanlation group.

### Screenshots (Anime)
- **Count**: **Minimum of 6 screenshots** (maximum of 9).
- **Dimensions**: Must match the exact resolution of the video file (no black bars, no cropping).
- **File Size**: Less than **20 MB** per screenshot (JPEGs around 200–500 KB recommended).
- **Subtitles in Screenshots**: If the release is hardsubbed, hardsubs **must be visible** in some screenshots. Softsubs are optional.
- **No Thumbnails / Video Sheets**: Video-sheets, collage grids, and thumbnail sheets are not permitted.
- **No Spoilers**: Avoid major story spoilers.
- **Original Captures Only**: Must be taken directly from the media file; do not copy screenshots from other websites.

### Screenshots (Printed Media)
- Minimum of 3 sample pages. No cropping or resizing (unless page exceeds 2MB).

### MediaInfo (Anime)
- **Full MediaInfo**: The complete, untruncated MediaInfo output must be included.
- **Language**: English.
- **Batches**: One representative MediaInfo file is sufficient (preferably not episode 1).
- **Tools**: `MediaInfo` for encodes/remuxes; `BDInfo` for raw Blu-ray discs.

---

## 7. Tracker Source Tag & Reseeding

> [!CAUTION]
> The AnimeZ tracker automatically adds its own `ANIMEZ` source tag (`source = "animez.to"`) and passkey announce URL to uploaded torrents, **changing the infohash**.
>
> 1. You **MUST** download the tracker's re-signed `.torrent` from `https://animez.to/torrents/<id>/download`.
> 2. The local `.torrent` created during packaging will have a different hash and will **not seed** on AnimeZ.
> 3. Inject the downloaded tracker `.torrent` into your client (qBittorrent) with fast resume (`skip_checking=true`) pointing to the exact same file path.
