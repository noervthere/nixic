//! Application state: the playback queue, search results, and all input
//! handling (keyboard + mouse).

use crate::cava::CavaVisualizer;
use crate::mpris::{self, MprisBridge, MprisCmd, Snapshot};
use crate::mpv::{Mpv, MpvEvent};
use crate::music::{ArtRes, MusicWorker, StreamInfo, ThumbFetcher, WorkReq, WorkRes};
use crate::theme;
use crate::track::{thumb_url, Track};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use rand::Rng;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Home,
    Albums,
    Artists,
    Playlists,
    Search,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "Off",
            RepeatMode::All => "All",
            RepeatMode::One => "One",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ShowHome,
    ShowAlbums,
    ShowArtists,
    ShowPlaylists,
    ShowSearch,
    TogglePlay,
    ToggleMute,
    ToggleVisualizer,
    CycleRepeat,
    ToggleShuffle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneKind {
    Button(Action),
    Progress,
    Volume,
    List,
}

/// A clickable region from the last rendered frame.
#[derive(Debug, Clone, Copy)]
pub struct Zone {
    pub kind: ZoneKind,
    pub rect: Rect,
}

#[derive(Debug)]
struct PendingPlay {
    id: u64,
    index: usize,
}

pub struct App {
    pub mode: Mode,
    pub queue: Vec<Track>,
    pub queue_cursor: usize,
    pub queue_offset: usize,
    pub current: Option<usize>,
    pub playing: bool,
    pub position: f64,
    pub duration: Option<f64>,
    pub volume: u8,
    /// Volume to restore when unmuting (0 = never muted yet).
    unmute_volume: u8,
    pub muted: bool,

    pub search_input: String,
    pub search_results: Vec<Track>,
    pub search_cursor: usize,
    pub search_offset: usize,
    pub search_loading: bool,
    pub search_error: Option<String>,
    last_search: String,

    pub status: String,
    pub hover: Option<ZoneKind>,
    pub zones: Vec<Zone>,

    pub repeat: RepeatMode,
    pub shuffle: bool,

    /// Encoded album art for the current track (ratatui-image stateful
    /// protocol — re-encodes itself when the art panel is resized).
    pub art_state: Option<StatefulProtocol>,
    /// Video id the loaded art belongs to (cache: skip refetching).
    art_for: Option<String>,
    picker: Picker,

    /// Visualizer pane enabled?
    pub viz_on: bool,
    /// Real audio visualizer powered by cava.
    pub cava: Option<CavaVisualizer>,
    /// cava failed to start (missing binary / no audio server). Latched so we
    /// don't retry a failing spawn every frame; reset by toggling viz off/on.
    cava_unavailable: bool,

    /// When the playback position last advanced (stall watchdog).
    last_position_change: Instant,
    /// When mpv was last restarted after dying (throttle respawns).
    last_mpv_respawn: Option<Instant>,
    /// When the theme config file was last stat'd + its mtime (hot reload).
    last_theme_check: Instant,
    theme_mtime: Option<SystemTime>,
    mpv_bin: String,

    last_click: Option<(Instant, u16, u16)>,

    mpv: Mpv,
    worker: MusicWorker,
    thumbs: ThumbFetcher,
    pending_play: Option<PendingPlay>,
    /// Resolve attempts made for the current track (fresh resolve each retry).
    resolve_attempts: u8,
    /// Whether mpv is sitting idle (stopped) rather than paused.
    mpv_idle: bool,
    mpris: MprisBridge,
    request_seq: u64,
    search_request_id: u64,
    pub should_quit: bool,
    /// Position (seconds) to seek to right after the next file loads — used
    /// to resume mid-track when a retry was triggered by a mid-play failure.
    seek_after_load: f64,
}

impl App {
    /// Max resolve+load attempts per track before we skip it.
    const MAX_PLAY_ATTEMPTS: u8 = 3;

    pub fn new(mpv_bin: &str, ytdlp_bin: &str, volume: u8, picker: Picker) -> Result<Self> {
        let mut mpv = Mpv::spawn(mpv_bin, volume)?;
        mpv.observe_all()?;
        Ok(Self {
            mode: Mode::Home,
            queue: Vec::new(),
            queue_cursor: 0,
            queue_offset: 0,
            current: None,
            playing: false,
            position: 0.0,
            duration: None,
            volume,
            unmute_volume: 70,
            muted: false,
            search_input: String::new(),
            search_results: Vec::new(),
            search_cursor: 0,
            search_offset: 0,
            search_loading: false,
            search_error: None,
            last_search: String::new(),
            status: "Welcome to nixic — press / or click [Search] to find music".into(),
            hover: None,
            zones: Vec::new(),
            repeat: RepeatMode::Off,
            shuffle: false,
            art_state: None,
            art_for: None,
            picker,
            viz_on: true,
            cava: CavaVisualizer::spawn("cava").ok(),
            cava_unavailable: false,
            last_position_change: Instant::now(),
            last_mpv_respawn: None,
            last_theme_check: Instant::now(),
            theme_mtime: std::fs::metadata(theme::config_path())
                .and_then(|m| m.modified())
                .ok(),
            mpv_bin: mpv_bin.to_string(),
            last_click: None,
            mpv,
            worker: MusicWorker::spawn(ytdlp_bin.to_string()),
            thumbs: ThumbFetcher::spawn(),
            pending_play: None,
            resolve_attempts: 0,
            mpv_idle: true,
            mpris: MprisBridge::spawn(),
            request_seq: 0,
            search_request_id: 0,
            should_quit: false,
            seek_after_load: 0.0,
        })
    }

    pub fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.queue.get(i))
    }

    /// Drain mpv events and worker results; called every frame.
    pub fn tick(&mut self) {
        if !self.mpv.is_alive() {
            self.handle_mpv_death();
        }
        loop {
            match self.mpv.try_event() {
                Ok(Some(ev)) => self.handle_mpv_event(ev),
                Ok(None) => break,
                Err(_) => {
                    self.status = "mpv disconnected — playback unavailable".into();
                    break;
                }
            }
        }
        // Respawn cava if it died while the visualizer is enabled. Once a
        // spawn fails (binary missing / no audio server) we latch
        // `cava_unavailable` so this stops retrying every frame — the user can
        // reset it by toggling the visualizer off and on again.
        if self.viz_on && !self.cava_unavailable {
            let needs_spawn = match &mut self.cava {
                Some(c) => !c.is_alive(),
                None => true,
            };
            if needs_spawn {
                match CavaVisualizer::spawn("cava") {
                    Ok(c) => self.cava = Some(c),
                    Err(_) => self.cava_unavailable = true,
                }
            }
        }
        self.watchdog_stall();
        if let Ok(Some(res)) = self.worker.try_recv() {
            match res {
                WorkRes::Search { id, result } => self.handle_search_result(id, result),
                WorkRes::Resolve { id, result } => self.handle_resolve_result(id, result),
            }
        }
        if let Ok(Some(res)) = self.thumbs.try_recv() {
            self.handle_art(res);
        }
        // Commands from MPRIS clients (media keys, desktop widgets).
        while let Some(cmd) = self.mpris.try_cmd() {
            self.handle_mpris_cmd(cmd);
        }
        self.check_theme_reload();
        // Publish our playback state to the MPRIS server thread.
        self.mpris.push(self.mpris_snapshot());
    }

    fn handle_art(&mut self, res: ArtRes) {
        let is_current = self
            .current_track()
            .map(|t| t.id.as_str())
            .unwrap_or_default()
            == res.video_id.as_str();
        match res.result {
            Ok(Some(img)) if is_current => {
                // Stateful protocol: re-encodes at render time to whatever
                // size the art panel currently is, so the cover stays sharp
                // as the terminal (or layout) changes.
                self.art_state = Some(self.picker.new_resize_protocol(img));
                self.art_for = Some(res.video_id);
            }
            _ => {} // fetch failed or track already changed — keep placeholder
        }
    }

    /// mpv died: restart it and resume the current track (fresh resolve).
    fn handle_mpv_death(&mut self) {
        if let Some(last) = self.last_mpv_respawn {
            if last.elapsed() < Duration::from_secs(2) {
                self.status = "mpv exited — restarting…".into();
                return;
            }
        }
        match Mpv::spawn(&self.mpv_bin, self.volume) {
            Ok(mut new_mpv) => {
                if new_mpv.observe_all().is_err() {
                    self.status = "mpv restart failed — playback unavailable".into();
                    return;
                }
                self.mpv = new_mpv;
                self.last_mpv_respawn = Some(Instant::now());
                let resume = self.current.is_some() && !self.mpv_idle;
                self.status = if resume {
                    "mpv restarted — resuming…".into()
                } else {
                    "mpv restarted".into()
                };
                if resume {
                    // Restart playback but resume mid-track from where mpv
                    // died, so a crash doesn't cut the song back to 0:00.
                    if let Some(i) = self.current {
                        let pos = self.position;
                        self.play_index(i);
                        if pos > 3.0 {
                            self.seek_after_load = pos;
                        }
                    }
                }
            }
            Err(e) => {
                self.last_mpv_respawn = Some(Instant::now());
                self.status = format!("mpv exited and could not be restarted ({e:#})");
            }
        }
    }

    /// If playback claims to run but the position never advances, treat it as
    /// a stall (mpv stuck buffering / no audio) and run the retry chain.
    /// Skipped while a resolve is in flight (that has its own 35s watchdog).
    fn watchdog_stall(&mut self) {
        let stalled = self.playing
            && !self.mpv_idle
            && self.pending_play.is_none()
            && self.current.is_some()
            && self.last_position_change.elapsed() >= Duration::from_secs(20);
        if stalled {
            self.status = "Playback stalled — retrying…".into();
            self.retry_or_give_up();
        }
    }

    /// Pick up theme config edits (checked every ~2s).
    fn check_theme_reload(&mut self) {
        if self.last_theme_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_theme_check = Instant::now();
        let path = theme::config_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime != self.theme_mtime {
            self.theme_mtime = mtime;
            match theme::reload() {
                Ok(()) => {
                    if self.theme_mtime.is_some() {
                        self.status = "Theme reloaded".into();
                    }
                }
                Err(e) => self.status = format!("Theme error: {e}"),
            }
        }
    }

    fn handle_mpv_event(&mut self, ev: MpvEvent) {
        match ev {
            MpvEvent::Position(p) => {
                if (p - self.position).abs() > 0.1 {
                    self.last_position_change = Instant::now();
                }
                self.position = p;
            }
            MpvEvent::Duration(d) => {
                self.duration = Some(d);
                if let Some(i) = self.current {
                    if let Some(t) = self.queue.get_mut(i) {
                        t.duration = Some(d);
                    }
                }
            }
            MpvEvent::Pause(p) => self.playing = !p,
            MpvEvent::Volume(v) => self.volume = v.clamp(0.0, 100.0) as u8,
            MpvEvent::FileLoaded => {
                self.playing = true;
                self.mpv_idle = false;
                // If this load is a retry that died mid-track, resume from
                // where it cut off instead of restarting from 0:00.
                if self.seek_after_load > 0.0 {
                    let target = self.seek_after_load;
                    self.seek_after_load = 0.0;
                    let _ = self.mpv.seek(target);
                    self.position = target;
                }
                let title = self.current_track().map(|t| t.title.clone());
                if let Some(title) = title {
                    self.status = format!("♪ {title}");
                }
            }
            MpvEvent::EndFile { reason } => self.handle_end_file(&reason),
        }
    }

    fn handle_end_file(&mut self, reason: &str) {
        match reason {
            "eof" => {
                if let Some(next) = self.next_index() {
                    self.play_index(next);
                } else {
                    self.current = None;
                    self.playing = false;
                    self.mpv_idle = true;
                    self.position = 0.0;
                    self.duration = None;
                    self.status = "End of queue".into();
                }
            }
            // A load failed (transient YouTube CDN 403, expired direct URL, …).
            // Re-resolve with a fresh yt-dlp call and retry; after
            // MAX_PLAY_ATTEMPTS, skip the broken track instead of stalling.
            "error" => self.retry_or_give_up(),
            // "stop" / "quit" / "unknown": the track was replaced by us
            // (or playback stopped); nothing to do.
            _ => {}
        }
    }

    pub fn play_index(&mut self, i: usize) {
        let Some(track) = self.queue.get(i).cloned() else {
            self.status = "Nothing to play".into();
            return;
        };
        self.current = Some(i);
        self.queue_cursor = i;
        self.position = 0.0;
        self.duration = None;
        self.playing = true;
        self.mpv_idle = false;
        self.resolve_attempts = 0;
        self.seek_after_load = 0.0; // fresh play starts from the top
        self.last_position_change = Instant::now();
        // Repeat-one is handled by mpv looping the file itself.
        let _ = self.mpv.set_loop(self.repeat == RepeatMode::One);
        self.request_seq += 1;
        let id = self.request_seq;
        self.pending_play = Some(PendingPlay { id, index: i });
        self.status = format!("Resolving stream… {}", track.title);
        if let Err(e) = self.worker.send(WorkReq::Resolve { url: track.url, id }) {
            self.status = format!("backend error: {e}");
            self.pending_play = None;
        }
        // Fetch album art in parallel (unless we already have it for this track).
        if self.art_for.as_deref() != Some(track.id.as_str()) {
            let _ = self.thumbs.send(&track.id);
        }
    }

    fn handle_resolve_result(&mut self, id: u64, result: Result<StreamInfo>) {
        let Some(pending) = self.pending_play.take() else {
            return;
        };
        if pending.id != id {
            return;
        }
        // Refresh the queue entry with the fresh metadata yt-dlp extracted
        // (search results from the flat merge often lack artist/duration).
        if let Ok(info) = &result {
            if let Some(t) = self.queue.get_mut(pending.index) {
                if !info.artist.is_empty() && info.artist != "Unknown Artist" {
                    t.artist = info.artist.clone();
                }
                if info.duration.is_some() {
                    t.duration = info.duration;
                }
                if !info.title.is_empty() {
                    t.title = info.title.clone();
                }
            }
        }
        let Some(track) = self.queue.get(pending.index).cloned() else {
            return;
        };

        // Two layers: yt-dlp gives us a direct stream URL (fast path); if that
        // fails to resolve *or* mpv refuses the direct URL, hand mpv the watch
        // URL and let its built-in yt-dlp integration resolve + play it. A
        // failure that only shows up later (end-file `error`) is handled by
        // `retry_or_give_up`, which re-resolves with a fresh URL.
        let direct = match result {
            Ok(info) => Some(info.url),
            Err(e) => {
                self.status = format!("Resolve failed ({e:#}), retrying via mpv…");
                None
            }
        };
        let loaded = match direct {
            Some(url) => match self.mpv.load_direct(&url) {
                Ok(()) => {
                    self.status = format!("Loading… {}", track.title);
                    true
                }
                Err(e) => {
                    self.status = format!("Direct stream failed ({e}), retrying via mpv…");
                    self.mpv.load_watch(&track.url).is_ok()
                }
            },
            None => self.mpv.load_watch(&track.url).is_ok(),
        };
        if !loaded {
            // Couldn't even start the load (mpv died / IPC socket error).
            self.retry_or_give_up();
        }
    }

    /// Re-resolve the current track's watch URL with a fresh yt-dlp call and
    /// try again (direct stream URLs expire and CDN 403s are transient). After
    /// `MAX_PLAY_ATTEMPTS` total attempts, skip the broken track.
    fn retry_or_give_up(&mut self) {
        let Some(cur) = self.current else {
            return;
        };
        let Some(track) = self.queue.get(cur).cloned() else {
            return;
        };
        if self.resolve_attempts < Self::MAX_PLAY_ATTEMPTS {
            self.resolve_attempts += 1;
            // Remember where we died so the retry resumes mid-track instead
            // of restarting the song from the top ("cut off mid play").
            if self.position > 3.0 {
                self.seek_after_load = self.position;
            }
            self.request_seq += 1;
            let id = self.request_seq;
            self.pending_play = Some(PendingPlay { id, index: cur });
            self.status = format!(
                "Playback error — retry {}/{} for “{}”…",
                self.resolve_attempts,
                Self::MAX_PLAY_ATTEMPTS,
                track.title
            );
            if self.worker.send(WorkReq::Resolve { url: track.url, id }).is_err() {
                self.status = "backend error".into();
                self.give_up_current();
            }
        } else {
            self.give_up_current();
        }
    }

    /// Give up on the current track: auto-advance to the next one in the
    /// queue, or stop cleanly if it was the last.
    fn give_up_current(&mut self) {
        let title = self
            .current_track()
            .map(|t| t.title.clone())
            .unwrap_or_default();
        if let Some(cur) = self.current {
            if cur + 1 < self.queue.len() {
                let next_title = self.queue[cur + 1].title.clone();
                self.play_index(cur + 1);
                self.status = format!("Skipped “{title}” (playback error) — now: {next_title}");
                return;
            }
        }
        self.current = None;
        self.playing = false;
        self.mpv_idle = true;
        self.position = 0.0;
        self.duration = None;
        self.pending_play = None;
        self.status = format!("Playback error: {title}");
    }

    fn handle_search_result(&mut self, id: u64, result: Result<Vec<Track>>) {
        if id != self.search_request_id {
            return; // stale result from an older query
        }
        self.search_loading = false;
        match result {
            Ok(tracks) => {
                self.search_results = tracks;
                self.search_cursor = 0;
                self.search_offset = 0;
                self.status = if self.search_results.is_empty() {
                    "No results found".into()
                } else {
                    format!("{} results — Enter to play, double-click to play", self.search_results.len())
                };
            }
            Err(e) => {
                self.search_error = Some(format!("{e:#}"));
                self.status = "Search failed".into();
            }
        }
    }

    // ----- actions -----

    pub fn trigger_action(&mut self, action: Action) {
        match action {
            Action::ShowHome => self.mode = Mode::Home,
            Action::ShowAlbums => self.mode = Mode::Albums,
            Action::ShowArtists => self.mode = Mode::Artists,
            Action::ShowPlaylists => self.mode = Mode::Playlists,
            Action::ShowSearch => self.mode = Mode::Search,
            Action::TogglePlay => self.toggle_play(),
            Action::ToggleMute => self.toggle_mute(),
            Action::ToggleVisualizer => self.toggle_visualizer(),
            Action::CycleRepeat => self.cycle_repeat(),
            Action::ToggleShuffle => self.toggle_shuffle(),
        }
    }

    pub fn toggle_play(&mut self) {
        match self.current {
            None => {
                if self.queue.is_empty() {
                    self.status = "Queue is empty — press / to search YouTube Music".into();
                } else {
                    self.play_index(self.queue_cursor);
                }
            }
            Some(i) => {
                if self.mpv_idle {
                    // mpv is stopped (Stop pressed / playback failed): restart.
                    self.play_index(i);
                } else {
                    self.playing = !self.playing;
                    if let Err(e) = self.mpv.set_pause(!self.playing) {
                        self.status = format!("mpv error: {e}");
                    }
                }
            }
        }
    }

    pub fn next(&mut self) {
        if self.current.is_none() {
            self.toggle_play();
            return;
        }
        match self.next_index() {
            Some(i) => self.play_index(i),
            None => self.status = "End of queue".into(),
        }
    }

    /// Where playback advances next: shuffle picks a random other track,
    /// repeat-all wraps to the start, otherwise the next queue slot.
    fn next_index(&self) -> Option<usize> {
        let cur = self.current?;
        if self.shuffle && self.queue.len() > 1 {
            let mut r = rand::thread_rng().gen_range(0..self.queue.len());
            let mut guard = 0;
            while r == cur && guard < 10 {
                r = rand::thread_rng().gen_range(0..self.queue.len());
                guard += 1;
            }
            return Some(r);
        }
        let n = cur + 1;
        if n < self.queue.len() {
            Some(n)
        } else if self.repeat == RepeatMode::All && !self.queue.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.cycle();
        let _ = self.mpv.set_loop(self.repeat == RepeatMode::One);
        self.status = format!("Repeat: {}", self.repeat.label());
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.status = if self.shuffle {
            "Shuffle: on".into()
        } else {
            "Shuffle: off".into()
        };
    }

    pub fn prev(&mut self) {
        let Some(cur) = self.current else {
            self.toggle_play();
            return;
        };
        if self.position > 3.0 {
            let _ = self.mpv.seek(0.0); // restart current track
        } else if cur > 0 {
            self.play_index(cur - 1);
        } else {
            let _ = self.mpv.seek(0.0);
        }
    }

    pub fn change_volume(&mut self, delta: i32) {
        let v = (self.volume as i32 + delta).clamp(0, 100) as u8;
        self.set_volume_abs(v);
    }

    /// Set the volume to an absolute 0..=100 value (unmuting as needed).
    pub fn set_volume_abs(&mut self, v: u8) {
        self.volume = v;
        self.muted = false;
        if let Err(e) = self.mpv.set_volume(v) {
            self.status = format!("mpv error: {e}");
        }
    }

    pub fn toggle_mute(&mut self) {
        if self.muted {
            self.set_volume_abs(self.unmute_volume.max(1));
            self.status = "Unmuted".into();
        } else {
            self.unmute_volume = self.volume;
            self.volume = 0;
            self.muted = true;
            if let Err(e) = self.mpv.set_volume(0) {
                self.status = format!("mpv error: {e}");
            }
            self.status = "Muted".into();
        }
    }

    pub fn toggle_visualizer(&mut self) {
        self.viz_on = !self.viz_on;
        if self.viz_on {
            // Give cava another chance (e.g. it was missing at startup).
            self.cava_unavailable = false;
            if self.cava.is_none() {
                self.cava = CavaVisualizer::spawn("cava").ok();
            }
            self.status = "Visualizer: on".into();
        } else {
            // Kill cava to save CPU when viz is off.
            self.cava = None;
            self.status = "Visualizer: off".into();
        }
    }

    /// Click on the bottom-bar volume slider: set volume by position.
    fn volume_from_click(&mut self, x: u16, rect: Rect) {
        let w = rect.width.max(1) as f64;
        let frac = ((x.saturating_sub(rect.x)) as f64 / w).clamp(0.0, 1.0);
        self.set_volume_abs((frac * 100.0).round() as u8);
    }

    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.current = None;
        self.playing = false;
        self.position = 0.0;
        self.duration = None;
        self.pending_play = None;
        self.mpv_idle = true;
        self.queue_cursor = 0;
        self.queue_offset = 0;
        let _ = self.mpv.stop();
        self.status = "Queue cleared".into();
    }

    /// Stop playback but keep the queue (used by MPRIS Stop and friends).
    pub fn stop_playback(&mut self) {
        self.playing = false;
        self.mpv_idle = true;
        self.position = 0.0;
        self.status = "Stopped".into();
        let _ = self.mpv.stop();
    }

    pub fn remove_selected(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let i = self.queue_cursor.min(self.queue.len() - 1);
        self.queue.remove(i);
        if self.current == Some(i) {
            self.current = None;
            self.playing = false;
            self.mpv_idle = true;
            self.position = 0.0;
            self.duration = None;
            self.pending_play = None;
            let _ = self.mpv.stop();
            self.status = "Stopped (track removed)".into();
        } else if let Some(cur) = self.current {
            if cur > i {
                self.current = Some(cur - 1);
            }
        }
        if !self.queue.is_empty() {
            self.queue_cursor = self.queue_cursor.min(self.queue.len() - 1);
        }
    }

    // ----- MPRIS -----

    /// Apply a command originating from an MPRIS client (media keys, widgets).
    fn handle_mpris_cmd(&mut self, cmd: MprisCmd) {
        match cmd {
            MprisCmd::Play => match self.current {
                None => self.toggle_play(),
                Some(i) => {
                    if self.mpv_idle {
                        self.play_index(i);
                    } else {
                        self.playing = true;
                        if let Err(e) = self.mpv.set_pause(false) {
                            self.status = format!("mpv error: {e}");
                        }
                    }
                }
            },
            MprisCmd::Pause => {
                if self.current.is_some() && !self.mpv_idle && self.playing {
                    self.playing = false;
                    if let Err(e) = self.mpv.set_pause(true) {
                        self.status = format!("mpv error: {e}");
                    }
                }
            }
            MprisCmd::PlayPause => self.toggle_play(),
            MprisCmd::Stop => self.stop_playback(),
            MprisCmd::Next => self.next(),
            MprisCmd::Previous => self.prev(),
            MprisCmd::Seek(rel) => self.seek_relative(rel),
            MprisCmd::SetPosition(abs) => self.seek_absolute(abs),
            MprisCmd::OpenUri(uri) => self.open_uri(&uri),
            MprisCmd::SetVolume(v) => {
                self.set_volume_abs((v.clamp(0.0, 1.0) * 100.0) as u8);
            }
            MprisCmd::SetLoop(code) => {
                self.repeat = match code {
                    2 => RepeatMode::One,
                    1 => RepeatMode::All,
                    _ => RepeatMode::Off,
                };
                let _ = self.mpv.set_loop(self.repeat == RepeatMode::One);
            }
            MprisCmd::SetShuffle(on) => self.shuffle = on,
            MprisCmd::Quit => self.should_quit = true,
        }
    }

    fn seek_relative(&mut self, rel: f64) {
        if self.current.is_none() {
            return;
        }
        self.seek_absolute(self.position + rel);
    }

    fn seek_absolute(&mut self, abs: f64) {
        if self.current.is_none() {
            return;
        }
        let target = abs.max(0.0);
        if let Err(e) = self.mpv.seek(target) {
            self.status = format!("mpv error: {e}");
        } else {
            self.position = target;
            self.last_position_change = Instant::now();
            self.mpris.seeked(target);
        }
    }

    /// Play a URI from an MPRIS client (YouTube watch URLs / bare video ids).
    fn open_uri(&mut self, uri: &str) {
        let Some(id) = mpris::extract_video_id(uri) else {
            self.status = format!("Unsupported MPRIS URI: {uri}");
            return;
        };
        let url = if uri.starts_with("http") {
            uri.to_string()
        } else {
            format!("https://www.youtube.com/watch?v={id}")
        };
        let track = Track {
            id,
            title: uri.to_string(),
            artist: "Unknown Artist".into(),
            duration: None,
            url,
        };
        if let Some(existing) = self.queue.iter().position(|t| t.id == track.id) {
            self.play_index(existing);
        } else {
            self.queue.push(track);
            self.play_index(self.queue.len() - 1);
        }
    }

    /// Build the MPRIS snapshot from the current app state.
    fn mpris_snapshot(&self) -> Snapshot {
        let track = self.current_track();
        let status = if track.is_none() {
            0
        } else if self.playing {
            1
        } else {
            2
        };
        let loop_status = match self.repeat {
            RepeatMode::Off => 0,
            RepeatMode::All => 1,
            RepeatMode::One => 2,
        };
        Snapshot {
            status,
            title: track.map(|t| t.title.clone()).unwrap_or_default(),
            artist: track.map(|t| t.artist.clone()).unwrap_or_default(),
            art_url: track.and_then(|t| thumb_url(&t.id)),
            url: track.map(|t| t.url.clone()),
            length_micros: self.duration.map(|d| (d * 1e6) as i64),
            track_id: track.map(|t| mpris::track_path(&t.id)).unwrap_or_default(),
            position_micros: (self.position * 1e6) as i64,
            volume: self.volume as f64 / 100.0,
            loop_status,
            shuffle: self.shuffle,
        }
    }

    pub fn run_search(&mut self) {
        let query = self.search_input.trim().to_string();
        if query.is_empty() {
            self.status = "Type a query first".into();
            return;
        }
        self.search_loading = true;
        self.search_error = None;
        self.last_search = query.clone();
        self.request_seq += 1;
        let id = self.request_seq;
        self.search_request_id = id;
        self.status = format!("Searching: {query}");
        if let Err(e) = self.worker.send(WorkReq::Search { query, id }) {
            self.status = format!("backend error: {e}");
            self.search_loading = false;
        }
    }

    pub fn play_selected(&mut self) {
        match self.mode {
            Mode::Home => {
                if !self.queue.is_empty() {
                    self.play_index(self.queue_cursor.min(self.queue.len() - 1));
                }
            }
            Mode::Search => {
                if !self.search_results.is_empty() {
                    let idx = self.search_cursor.min(self.search_results.len() - 1);
                    let track = self.search_results[idx].clone();
                    // Avoid duplicates: jump to the existing queue entry instead.
                    if let Some(existing) = self.queue.iter().position(|t| t.id == track.id) {
                        self.play_index(existing);
                    } else {
                        self.queue.push(track);
                        self.play_index(self.queue.len() - 1);
                    }
                    self.mode = Mode::Home;
                }
            }
            Mode::Help | Mode::Albums | Mode::Artists | Mode::Playlists => {}
        }
    }

    // ----- keyboard -----

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.mode {
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Char('?') => {
                    self.mode = Mode::Home;
                }
                _ => {}
            },
            Mode::Search => match key.code {
                KeyCode::Esc => self.mode = Mode::Home,
                KeyCode::Enter => {
                    // Enter runs the search, or plays the selected result once
                    // results are loaded and the query hasn't changed.
                    if self.search_input.trim() != self.last_search || self.search_results.is_empty()
                    {
                        self.run_search();
                    } else {
                        self.play_selected();
                    }
                }
                KeyCode::Char(c) => self.search_input.push(c),
                KeyCode::Backspace => {
                    self.search_input.pop();
                }
                KeyCode::Up => self.move_up(),
                KeyCode::Down => self.move_down(),
                KeyCode::PageUp => self.page_up(),
                KeyCode::PageDown => self.page_down(),
                _ => {}
            },
            Mode::Home => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('/') => self.mode = Mode::Search,
                KeyCode::Enter => self.play_selected(),
                KeyCode::Char(' ') => self.toggle_play(),
                KeyCode::Char('n') => self.next(),
                KeyCode::Char('b') => self.prev(),
                KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(5),
                KeyCode::Char('-') | KeyCode::Char('_') => self.change_volume(-5),
                KeyCode::Char('m') => self.toggle_mute(),
                KeyCode::Char('v') => self.toggle_visualizer(),
                KeyCode::Char('h') | KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('c') => self.clear_queue(),
                KeyCode::Char('d') => self.remove_selected(),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match theme::reload() {
                        Ok(()) => self.status = "Theme reloaded".into(),
                        Err(e) => self.status = format!("Theme error: {e}"),
                    }
                }
                KeyCode::Char('r') => self.cycle_repeat(),
                KeyCode::Char('s') => self.toggle_shuffle(),
                KeyCode::Up | KeyCode::Char('k') => self.move_up(),
                KeyCode::Down | KeyCode::Char('j') => self.move_down(),
                KeyCode::PageUp => self.page_up(),
                KeyCode::PageDown => self.page_down(),
                _ => {}
            },
            // Albums / Artists / Playlists: placeholder views until the
            // browsing backends land; Esc or h returns Home.
            Mode::Albums | Mode::Artists | Mode::Playlists => match key.code {
                KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('?') => {
                    self.mode = Mode::Home;
                }
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            },
        }
    }

    // ----- mouse -----

    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        let x = ev.column;
        let y = ev.row;
        match ev.kind {
            MouseEventKind::Moved => {
                self.hover = self.zone_at(x, y).map(|z| z.kind);
            }
            MouseEventKind::ScrollUp => self.move_up(),
            MouseEventKind::ScrollDown => self.move_down(),
            MouseEventKind::Down(MouseButton::Left) => {
                // crossterm has no double-click event; detect it ourselves.
                let double = matches!(
                    self.last_click,
                    Some((t, lx, ly)) if t.elapsed() < Duration::from_millis(400) && lx == x && ly == y
                );
                self.last_click = Some((Instant::now(), x, y));
                if let Some(zone) = self.zone_at(x, y) {
                    match zone.kind {
                        ZoneKind::Button(a) => self.trigger_action(a),
                        ZoneKind::Progress => self.seek_to(x, zone.rect),
                        ZoneKind::Volume => self.volume_from_click(x, zone.rect),
                        ZoneKind::List => {
                            self.click_row(y, zone.rect);
                            if double {
                                self.play_selected();
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn zone_at(&self, x: u16, y: u16) -> Option<Zone> {
        self.zones
            .iter()
            .copied()
            .find(|z| rect_contains(z.rect, x, y))
    }

    fn click_row(&mut self, y: u16, rect: Rect) {
        if y < rect.y || y >= rect.y + rect.height {
            return;
        }
        let row = (y - rect.y) as usize;
        match self.mode {
            Mode::Home => {
                let idx = self.queue_offset + row;
                if idx < self.queue.len() {
                    self.queue_cursor = idx;
                }
            }
            Mode::Search => {
                let idx = self.search_offset + row;
                if idx < self.search_results.len() {
                    self.search_cursor = idx;
                }
            }
            Mode::Help | Mode::Albums | Mode::Artists | Mode::Playlists => {}
        }
    }

    fn seek_to(&mut self, x: u16, rect: Rect) {
        let Some(dur) = self.duration.filter(|d| *d > 0.0) else {
            return;
        };
        let w = rect.width.max(1) as f64;
        let frac = ((x.saturating_sub(rect.x)) as f64 / w).clamp(0.0, 1.0);
        self.seek_absolute(frac * dur);
    }

    fn move_up(&mut self) {
        match self.mode {
            Mode::Home => self.queue_cursor = self.queue_cursor.saturating_sub(1),
            Mode::Search => self.search_cursor = self.search_cursor.saturating_sub(1),
            Mode::Help | Mode::Albums | Mode::Artists | Mode::Playlists => {}
        }
    }

    fn move_down(&mut self) {
        match self.mode {
            Mode::Home => {
                if self.queue_cursor + 1 < self.queue.len() {
                    self.queue_cursor += 1;
                }
            }
            Mode::Search => {
                if self.search_cursor + 1 < self.search_results.len() {
                    self.search_cursor += 1;
                }
            }
            Mode::Help | Mode::Albums | Mode::Artists | Mode::Playlists => {}
        }
    }

    fn page_up(&mut self) {
        match self.mode {
            Mode::Home => self.queue_cursor = self.queue_cursor.saturating_sub(10),
            Mode::Search => self.search_cursor = self.search_cursor.saturating_sub(10),
            Mode::Help | Mode::Albums | Mode::Artists | Mode::Playlists => {}
        }
    }

    fn page_down(&mut self) {
        self.move_down_n(10);
    }

    fn move_down_n(&mut self, n: usize) {
        match self.mode {
            Mode::Home => {
                self.queue_cursor = (self.queue_cursor + n).min(self.queue.len().saturating_sub(1));
            }
            Mode::Search => {
                self.search_cursor = (self.search_cursor + n)
                    .min(self.search_results.len().saturating_sub(1));
            }
            Mode::Help | Mode::Albums | Mode::Artists | Mode::Playlists => {}
        }
    }

    /// Keep the selected row visible given the rendered list heights.
    pub fn update_scroll(&mut self, queue_height: usize, search_height: usize) {
        self.queue_offset =
            ensure_visible(self.queue_cursor, self.queue_offset, queue_height);
        self.search_offset =
            ensure_visible(self.search_cursor, self.search_offset, search_height);
    }

}

pub fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn ensure_visible(cursor: usize, offset: usize, height: usize) -> usize {
    if height == 0 {
        return offset;
    }
    if cursor < offset {
        cursor
    } else if cursor >= offset + height {
        cursor + 1 - height
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_stays_in_view() {
        assert_eq!(ensure_visible(0, 0, 5), 0);
        assert_eq!(ensure_visible(9, 0, 5), 5);
        assert_eq!(ensure_visible(12, 5, 5), 8);
        assert_eq!(ensure_visible(7, 5, 5), 5);
        assert_eq!(ensure_visible(3, 10, 5), 3);
    }

    #[test]
    fn rect_hit_testing() {
        let r = Rect::new(0, 0, 10, 5);
        assert!(rect_contains(r, 0, 0));
        assert!(rect_contains(r, 9, 4));
        assert!(!rect_contains(r, 10, 0));
        assert!(!rect_contains(r, 0, 5));
    }
}
