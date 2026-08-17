//! Music backend built on top of `yt-dlp`.
//!
//! There is no official public YouTube Music API, so we shell out to yt-dlp,
//! which knows how to talk to YouTube Music's internal API and is the
//! de-facto interface used by every TUI music player. Search and stream
//! resolution run on a background worker thread so the UI never blocks.
//!
//! The `MusicWorker` API is the seam where a native InnerTube client could
//! later replace the yt-dlp process without touching the UI.

use crate::track::{thumb_urls, urlencode, Track};
use anyhow::{bail, Context, Result};
use image::DynamicImage;
use serde_json::Value;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum WorkReq {
    Search { query: String, id: u64 },
    Resolve { url: String, id: u64 },
}

/// A resolved stream plus the fresh metadata yt-dlp extracted for it.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub url: String,
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
}

#[derive(Debug)]
pub enum WorkRes {
    Search { id: u64, result: Result<Vec<Track>> },
    Resolve { id: u64, result: Result<StreamInfo> },
}

pub struct MusicWorker {
    search_tx: Sender<WorkReq>,
    resolve_tx: Sender<WorkReq>,
    rx: Receiver<WorkRes>,
}

impl MusicWorker {
    /// Spawn **separate** worker threads for search and resolve. On a slow or
    /// flaky network a search can take a while (and requests queue up behind
    /// it); routing resolves on their own thread means "play" never waits for
    /// a search backlog.
    pub fn spawn(bin: String) -> Self {
        let (res_tx, res_rx) = channel::<WorkRes>();
        let (search_tx, search_rx) = channel::<WorkReq>();
        let (resolve_tx, resolve_rx) = channel::<WorkReq>();
        let s_bin = bin.clone();
        let search_res_tx = res_tx.clone();
        thread::spawn(move || {
            for req in search_rx {
                if matches!(req, WorkReq::Search { .. })
                    && search_res_tx.send(handle(&s_bin, req)).is_err()
                {
                    break;
                }
            }
        });
        thread::spawn(move || {
            for req in resolve_rx {
                if matches!(req, WorkReq::Resolve { .. })
                    && res_tx.send(handle(&bin, req)).is_err()
                {
                    break;
                }
            }
        });
        Self {
            search_tx,
            resolve_tx,
            rx: res_rx,
        }
    }

    pub fn send(&self, req: WorkReq) -> Result<()> {
        let tx = match &req {
            WorkReq::Search { .. } => &self.search_tx,
            WorkReq::Resolve { .. } => &self.resolve_tx,
        };
        tx.send(req).map_err(Into::into)
    }

    /// Non-blocking drain of finished work items.
    pub fn try_recv(&self) -> Result<Option<WorkRes>> {
        match self.rx.try_recv() {
            Ok(res) => Ok(Some(res)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(_) => bail!("music backend worker died"),
        }
    }
}

fn handle(bin: &str, req: WorkReq) -> WorkRes {
    match req {
        WorkReq::Search { query, id } => WorkRes::Search {
            id,
            result: search_merged(bin, &query),
        },
        WorkReq::Resolve { url, id } => WorkRes::Resolve {
            id,
            result: resolve_stream(bin, &url),
        },
    }
}

// ----- album art -----

pub struct ArtReq {
    pub video_id: String,
}

pub struct ArtRes {
    pub video_id: String,
    pub result: Result<Option<DynamicImage>>,
}

/// Fetches and downscales album art on its own thread (HTTP + JPEG decode is
/// slow enough that it must never block the UI). Failures are silent: the UI
/// falls back to a placeholder box.
pub struct ThumbFetcher {
    tx: Sender<ArtReq>,
    rx: Receiver<ArtRes>,
}

impl ThumbFetcher {
    pub fn spawn() -> Self {
        let (tx, rx) = channel::<ArtReq>();
        let (res_tx, res_rx) = channel::<ArtRes>();
        thread::spawn(move || {
            for req in rx {
                let result = fetch_thumb(&req.video_id);
                if res_tx
                    .send(ArtRes {
                        video_id: req.video_id,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { tx, rx: res_rx }
    }

    pub fn send(&self, video_id: &str) -> Result<()> {
        self.tx
            .send(ArtReq {
                video_id: video_id.to_string(),
            })
            .map_err(Into::into)
    }

    pub fn try_recv(&self) -> Result<Option<ArtRes>> {
        match self.rx.try_recv() {
            Ok(res) => Ok(Some(res)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(_) => bail!("thumbnail worker died"),
        }
    }
}

fn fetch_thumb(video_id: &str) -> Result<Option<DynamicImage>> {
    let urls = thumb_urls(video_id);
    if urls.is_empty() {
        return Ok(None);
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .build();
    // Try each resolution in descending quality order; fall back on HTTP errors
    // (YouTube returns 404 for maxresdefault on many videos).
    for url in &urls {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut body = Vec::new();
                resp.into_reader().take(4 * 1024 * 1024).read_to_end(&mut body)?;
                let img = image::load_from_memory(&body)?;
                return Ok(Some(img));
            }
            Err(ureq::Error::Status(404, _)) | Err(ureq::Error::Status(403, _)) => continue,
            Err(e) => {
                // Network error — try the next URL anyway.
                if url == urls.last().unwrap() {
                    return Err(e.into());
                }
                continue;
            }
        }
    }
    Ok(None)
}

/// How many still-unknown-artist results get a full metadata extraction.
const ENRICH_MAX: usize = 4;

/// Search both YouTube Music and plain YouTube in parallel and merge the
/// results. Music search is better at ranking actual songs; plain YouTube
/// search returns the same videos with richer metadata (artist, duration)
/// and catches tracks that never made it into the YT Music catalog.
///
/// Music-search entries carry *no* artist/duration in flat mode, so the
/// merged list gets two enrichment passes: a fast title-match fill from the
/// plain-YouTube results, then authoritative `yt-dlp -J` extractions for the
/// few tracks that are still unknown (parallel, ~1-2s).
fn search_merged(bin: &str, query: &str) -> Result<Vec<Track>> {
    let music_url = format!("https://music.youtube.com/search?q={}", urlencode(query));
    let yt_url = format!("ytsearch30:{query}");
    let music_bin = bin.to_string();
    let yt_bin = bin.to_string();

    let music = std::thread::spawn(move || run_search(&music_bin, &[music_url]));
    let yt = std::thread::spawn(move || run_search(&yt_bin, &[yt_url]));
    let music = music.join().unwrap_or_else(|_| Err(anyhow::anyhow!("search thread panicked")));
    let yt = yt.join().unwrap_or_else(|_| Err(anyhow::anyhow!("search thread panicked")));

    // Music results keep their position; where the same video also appeared
    // in the plain search we prefer the richer (artist + duration) copy.
    let music_tracks = music.unwrap_or_default();
    let yt_tracks = yt.unwrap_or_default();
    let merged = merge_tracks(music_tracks, yt_tracks);
    let filled = title_fill(merged);
    Ok(enrich_unknowns(bin, filled))
}

/// Combine music and plain-YouTube results: keep music positions, replace
/// duplicates with the richer plain-YouTube copy, then append the leftover
/// plain-YouTube-only results. Caps at 30.
fn merge_tracks(music: Vec<Track>, yt: Vec<Track>) -> Vec<Track> {
    let yt_by_id: std::collections::HashMap<String, Track> =
        yt.into_iter().map(|t| (t.id.clone(), t)).collect();
    let mut merged = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for t in music {
        let enriched = yt_by_id.get(&t.id).cloned().unwrap_or(t);
        if seen.insert(enriched.id.clone()) {
            merged.push(enriched);
        }
    }
    for t in yt_by_id.into_values() {
        if seen.insert(t.id.clone()) {
            merged.push(t);
        }
    }
    merged.truncate(30);
    merged
}

/// Best-effort artist fill for music-only results: if the music title
/// strongly matches a known-artist result's title (normalized token Jaccard
/// >= 0.5), reuse that artist and duration.
// Title-matching candidate: normalized tokens, artist, and duration.
type TitleCandidate = (Vec<String>, String, Option<f64>);

fn title_fill(mut tracks: Vec<Track>) -> Vec<Track> {
    let candidates: Vec<TitleCandidate> = tracks
        .iter()
        .filter(|t| t.artist != "Unknown Artist")
        .map(|t| (norm_tokens(&t.title), t.artist.clone(), t.duration))
        .collect();
    for t in tracks.iter_mut() {
        if t.artist != "Unknown Artist" {
            continue;
        }
        let a = norm_tokens(&t.title);
        let mut best: Option<(f64, &TitleCandidate)> = None;
        for c in &candidates {
            if a.is_empty() || c.0.is_empty() {
                continue;
            }
            let s = jaccard(&a, &c.0);
            if s >= 0.5 && best.map(|(bs, _)| s > bs).unwrap_or(true) {
                best = Some((s, c));
            }
        }
        if let Some((_, (_, artist, dur))) = best {
            t.artist = artist.clone();
            if t.duration.is_none() {
                t.duration = *dur;
            }
        }
    }
    tracks
}

/// Authoritative enrichment: run a full `yt-dlp -J` extraction for up to
/// `ENRICH_MAX` still-unknown results, in parallel, and patch them in place.
fn enrich_unknowns(bin: &str, mut tracks: Vec<Track>) -> Vec<Track> {
    let indices: Vec<usize> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.artist == "Unknown Artist")
        .map(|(i, _)| i)
        .take(ENRICH_MAX)
        .collect();
    if indices.is_empty() {
        return tracks;
    }
    let handles: Vec<_> = indices
        .iter()
        .map(|&i| {
            let bin = bin.to_string();
            let url = tracks[i].url.clone();
            std::thread::spawn(move || (i, run_resolve(&bin, &url, 10)))
        })
        .collect();
    for h in handles {
        if let Ok((i, Ok(info))) = h.join() {
            if info.artist != "Unknown Artist" {
                tracks[i].artist = info.artist;
            }
            if tracks[i].duration.is_none() {
                tracks[i].duration = info.duration;
            }
            if !info.title.is_empty() && info.title != tracks[i].title {
                tracks[i].title = info.title;
            }
        }
    }
    tracks
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "of", "and", "or", "in", "on", "to", "for", "with", "it", "its",
    "is", "at", "by", "from", "feat", "ft", "official", "video", "audio", "remastered",
];

/// Lowercased alphanumeric tokens with common filler words removed.
fn norm_tokens(title: &str) -> Vec<String> {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Token-set similarity (Jaccard index) between two normalized titles.
fn jaccard(a: &[String], b: &[String]) -> f64 {
    let ia: std::collections::HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let ib: std::collections::HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let inter = ia.intersection(&ib).count();
    let union = ia.union(&ib).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

fn run_search(bin: &str, targets: &[String]) -> Result<Vec<Track>> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "--flat-playlist",
        "--no-warnings",
        "--socket-timeout",
        "10",
        "--playlist-end",
        "30",
        "-J",
    ]);
    for t in targets {
        cmd.arg(t);
    }
    let out = cmd.output().with_context(|| format!("failed to run {bin}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("yt-dlp search failed: {}", stderr.trim());
    }
    let json: Value =
        serde_json::from_slice(&out.stdout).context("yt-dlp returned invalid JSON")?;
    parse_entries(&json)
}

fn parse_entries(json: &Value) -> Result<Vec<Track>> {
    let entries = json
        .get("entries")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    let mut tracks = Vec::new();
    for e in &entries {
        // Skip artist / album / playlist tabs that appear in music search.
        let ie_key = e.get("ie_key").and_then(|v| v.as_str()).unwrap_or("");
        if ie_key == "YoutubeTab" {
            continue;
        }
        let Some(id) = e.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if id.len() < 11 {
            continue;
        }
        // Channel / playlist / album identifiers, not standalone videos.
        if id.starts_with("UC") || id.starts_with("PL") || id.starts_with("OLAK") {
            continue;
        }
        let title = e
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown title")
            .to_string();
        let artist = extract_artist(e);
        let duration = e.get("duration").and_then(|v| v.as_f64());
        let url = match e.get("url").and_then(|v| v.as_str()) {
            Some(u) if u.contains("watch?v=") => u.to_string(),
            _ => format!("https://www.youtube.com/watch?v={id}"),
        };
        tracks.push(Track {
            id: id.to_string(),
            title,
            artist,
            duration,
            url,
        });
    }
    Ok(tracks)
}

fn extract_artist(e: &Value) -> String {
    if let Some(artists) = e.get("artists").and_then(|v| v.as_array()) {
        let names: Vec<&str> = artists
            .iter()
            .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
            .collect();
        if !names.is_empty() {
            return names.join(", ");
        }
    }
    for key in ["artist", "channel", "uploader"] {
        if let Some(v) = e.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return clean_artist(v);
            }
        }
    }
    "Unknown Artist".to_string()
}

/// Strip the " - Topic" suffix YouTube adds to auto-generated artist channels
/// (e.g. "Radiohead - Topic" -> "Radiohead").
fn clean_artist(name: &str) -> String {
    name.strip_suffix(" - Topic").unwrap_or(name).trim().to_string()
}

/// Resolve a watch URL to a direct audio stream URL (plus the metadata yt-dlp
/// extracted along the way — artist, title, duration), with a hard watchdog so
/// a flaky network can never wedge the worker forever.
fn resolve_stream(bin: &str, url: &str) -> Result<StreamInfo> {
    run_resolve(bin, url, 35)
}

/// Shared implementation of `yt-dlp -J` with a configurable watchdog (seconds).
fn run_resolve(bin: &str, url: &str, timeout_secs: u64) -> Result<StreamInfo> {
    let mut child = Command::new(bin)
        .args([
            "--no-playlist",
            "--no-warnings",
            "--socket-timeout",
            "10",
            "--retries",
            "2",
            "-f",
            "bestaudio/best",
            "-J",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {bin}"))?;
    let mut stdout = child.stdout.take().context("yt-dlp stdout")?;
    let mut stderr = child.stderr.take().context("yt-dlp stderr")?;

    let (tx, rx) = channel::<Result<StreamInfo>>();
    thread::spawn(move || {
        let mut out = String::new();
        let mut err = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = stderr.read_to_string(&mut err);
        let res = parse_stream_info(&out)
            .map_err(|e| anyhow::anyhow!("{e:#} (yt-dlp stderr: {})", err.trim()));
        let _ = tx.send(res);
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(res) => res,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("stream resolution timed out ({timeout_secs}s)")
        }
    }
}

fn parse_stream_info(out: &str) -> Result<StreamInfo> {
    let v: Value = serde_json::from_str(out).context("yt-dlp returned invalid JSON")?;
    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .context("no stream URL in yt-dlp output")?
        .to_string();
    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("Unknown title")
        .to_string();
    let artist = v
        .get("artist")
        .and_then(|a| a.as_str())
        .map(clean_artist)
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let duration = v.get("duration").and_then(|d| d.as_f64());
    Ok(StreamInfo {
        url,
        title,
        artist,
        duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_music_search_entries_and_skips_tabs() {
        // Shape captured from a live `yt-dlp` music.youtube.com search.
        let json = json!({
            "entries": [
                { "title": "Creep", "ie_key": "Youtube", "id": "9RfVp-GhKfs",
                  "_type": "url", "url": "https://music.youtube.com/watch?v=9RfVp-GhKfs" },
                { "ie_key": "YoutubeTab", "id": "UCr_iyUANcn9OX_yy9piYoLw",
                  "_type": "url", "url": "https://music.youtube.com/browse/UCr_iyUANcn9OX_yy9piYoLw" },
                { "ie_key": "YoutubeTab", "id": "MPREb_TgQPwAzodvg",
                  "_type": "url", "url": "https://music.youtube.com/browse/MPREb_TgQPwAzodvg" },
                { "title": "Creep (Acoustic)", "ie_key": "Youtube", "id": "4BX5xpB2DBM",
                  "_type": "url", "url": "https://music.youtube.com/watch?v=4BX5xpB2DBM" },
                { "title": "Creep (Very 2021 Thom Yorke Rmx)", "ie_key": "Youtube",
                  "id": "5R0c3TBwlfE", "_type": "url",
                  "url": "https://music.youtube.com/watch?v=5R0c3TBwlfE" }
            ]
        });
        let tracks = parse_entries(&json).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].title, "Creep");
        assert_eq!(tracks[0].id, "9RfVp-GhKfs");
        assert!(tracks[0].url.contains("watch?v="));
    }

    #[test]
    fn extracts_artist_from_various_fields() {
        let e = json!({ "artists": [{"name": "Radiohead"}, {"name": "Someone"}] });
        assert_eq!(extract_artist(&e), "Radiohead, Someone");
        let e = json!({ "uploader": "NPR Music" });
        assert_eq!(extract_artist(&e), "NPR Music");
        let e = json!({ "channel": "Fried By Fluoride - Topic" });
        assert_eq!(extract_artist(&e), "Fried By Fluoride");
        assert_eq!(extract_artist(&json!({})), "Unknown Artist");
    }

    #[test]
    fn parses_resolved_stream_info() {
        let json = json!({
            "url": "https://rr1.googlevideo.com/videoplayback?expire=1",
            "title": "Creep",
            "artist": "Radiohead",
            "duration": 239
        });
        let info = parse_stream_info(&json.to_string()).unwrap();
        assert_eq!(info.url, "https://rr1.googlevideo.com/videoplayback?expire=1");
        assert_eq!(info.artist, "Radiohead");
        assert_eq!(info.duration, Some(239.0));
        // " - Topic" suffix stripped
        let json = json!({"url": "u", "artist": "Nirvana - Topic"});
        let info = parse_stream_info(&json.to_string()).unwrap();
        assert_eq!(info.artist, "Nirvana");
    }

    #[test]
    fn title_similarity_scoring() {
        let a = norm_tokens("Creep");
        let b = norm_tokens("Radiohead - Creep");
        let c = norm_tokens("Radiohead - Creep (Lyrics)");
        assert_eq!(jaccard(&a, &b), 0.5);
        assert!(jaccard(&a, &c) < 0.5);
        assert_eq!(jaccard(&a, &a), 1.0);
        assert_eq!(norm_tokens("The A Team"), vec!["team"]);
    }

    #[test]
    fn title_fill_finds_artist_by_title() {
        let t = |id: &str, title: &str, artist: &str, dur: Option<f64>| Track {
            id: id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            duration: dur,
            url: format!("https://www.youtube.com/watch?v={id}"),
        };
        let tracks = vec![
            t("music1", "Creep", "Unknown Artist", None),
            t("yt1", "Radiohead - Creep", "Radiohead", Some(237.0)),
            t("yt2", "Radiohead - Creep (Lyrics)", "LyricsZone", Some(241.0)),
        ];
        let filled = title_fill(tracks);
        // Best title match wins: "Radiohead - Creep" (Jaccard 0.5), not the
        // Lyrics version (0.33) — so the artist is the band, not a fan channel.
        assert_eq!(filled[0].artist, "Radiohead");
        assert_eq!(filled[0].duration, Some(237.0));
        // Known-artist entries are untouched.
        assert_eq!(filled[1].artist, "Radiohead");
        assert_eq!(filled[2].artist, "LyricsZone");
    }

    #[test]
    fn merge_enriches_duplicates_and_appends_yt_only() {
        let t = |id: &str, artist: &str| Track {
            id: id.to_string(),
            title: "t".into(),
            artist: artist.to_string(),
            duration: Some(100.0),
            url: format!("https://www.youtube.com/watch?v={id}"),
        };
        // Same video in both: music copy has no artist, plain-YouTube copy does.
        let music = vec![t("aaa", "Unknown Artist"), t("bbb", "Unknown Artist")];
        let yt = vec![t("aaa", "Radiohead"), t("ccc", "Nirvana")];
        let merged = merge_tracks(music, yt);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].artist, "Radiohead"); // enriched in place
        assert_eq!(merged[1].artist, "Unknown Artist"); // music-only copy kept
        assert_eq!(merged[2].artist, "Nirvana"); // plain-YouTube-only appended
    }
}
