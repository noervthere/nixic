//! MPRIS (Media Player Remote Interfacing Specification) integration.
//!
//! nixic advertises `org.mpris.MediaPlayer2.nixic` on the D-Bus session bus so
//! desktop environments (GNOME Shell, KDE Plasma, …) can show the current
//! track and control playback with media keys or their media widgets.
//!
//! The TUI owns all playback state on the main thread. This module bridges it
//! to a dedicated MPRIS server thread:
//!
//! * the app pushes a [`Snapshot`] every frame through a shared mutex; the
//!   server thread diffs it against what it last advertised and emits
//!   `PropertiesChanged` for anything new (plus `Seeked` after seeks);
//! * D-Bus method calls (play / pause / next / seek / …) are forwarded to the
//!   app through an mpsc channel that `App::tick` drains every frame.
//!
//! If there is no session bus (headless, over ssh, …) the server fails to
//! start and the bridge becomes a silent no-op. Set `NIXIC_MPRIS=0` to
//! disable MPRIS explicitly.

use mpris_server::{zbus, LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId, Volume};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A command from an MPRIS client, forwarded to the app's main thread.
#[derive(Debug, Clone)]
pub enum MprisCmd {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Relative offset in seconds (may be negative).
    Seek(f64),
    /// Absolute position in seconds.
    SetPosition(f64),
    OpenUri(String),
    /// Volume in 0.0..=1.0.
    SetVolume(f64),
    /// Loop status: 0 = off, 1 = playlist, 2 = track.
    SetLoop(u8),
    SetShuffle(bool),
    Quit,
}

/// Playback state the app publishes every frame.
///
/// `status` is `0` = stopped, `1` = playing, `2` = paused. The server thread
/// diffs this against what it last advertised and only emits property changes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub status: u8,
    pub title: String,
    pub artist: String,
    pub art_url: Option<String>,
    pub url: Option<String>,
    pub length_micros: Option<i64>,
    /// Unique id per track (used to build `mpris:trackid`).
    pub track_id: String,
    pub position_micros: i64,
    /// Volume in 0.0..=1.0.
    pub volume: f64,
    /// Loop status: 0 = off, 1 = playlist, 2 = track.
    pub loop_status: u8,
    pub shuffle: bool,
}

struct Shared {
    snap: Snapshot,
    /// Microseconds of the most recent app-side seek (emitted as `Seeked`).
    seeked_micros: Option<i64>,
}

/// Handle held by the app: forwards MPRIS commands and publishes state.
pub struct MprisBridge {
    rx: Receiver<MprisCmd>,
    shared: Arc<Mutex<Shared>>,
}

impl MprisBridge {
    /// Start the MPRIS server on a background thread. Never fails: without a
    /// session bus (or with `NIXIC_MPRIS=0`) the server simply never comes up
    /// and the bridge stays inert.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = channel::<MprisCmd>();
        let shared = Arc::new(Mutex::new(Shared {
            snap: Snapshot::default(),
            seeked_micros: None,
        }));
        spawn_server(Arc::new(cmd_tx), shared.clone());
        Self { rx: cmd_rx, shared }
    }

    /// Publish the current playback state (cheap; the server thread diffs).
    pub fn push(&self, snap: Snapshot) {
        if let Ok(mut s) = self.shared.lock() {
            s.snap = snap;
        }
    }

    /// Note a seek performed by the app so the server emits `Seeked`.
    pub fn seeked(&self, secs: f64) {
        if let Ok(mut s) = self.shared.lock() {
            s.seeked_micros = Some((secs * 1e6) as i64);
        }
    }

    /// Non-blocking poll for a command from an MPRIS client.
    pub fn try_cmd(&self) -> Option<MprisCmd> {
        self.rx.try_recv().ok()
    }
}

/// How often the server thread re-reads the app's snapshot and emits updates.
const PUSH_INTERVAL: Duration = Duration::from_millis(250);

fn spawn_server(cmd_tx: Arc<Sender<MprisCmd>>, shared: Arc<Mutex<Shared>>) {
    if std::env::var("NIXIC_MPRIS").as_deref() == Ok("0") {
        return;
    }
    thread::Builder::new()
        .name("nixic-mpris".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[nixic] MPRIS: runtime error: {e}");
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let player = match Player::builder("nixic")
                    .identity("nixic")
                    .desktop_entry("nixic")
                    .can_play(true)
                    .can_pause(true)
                    .can_seek(true)
                    .can_go_next(true)
                    .can_go_previous(true)
                    .can_quit(true)
                    .can_control(true)
                    .build()
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[nixic] MPRIS unavailable (no session bus?): {e}");
                        return;
                    }
                };
                connect(&player, cmd_tx);
                // Must be awaited/kept alive ASAP; `spawn_local` runs it on
                // this thread while our push loop sleeps below.
                let _task = tokio::task::spawn_local(player.run());
                let mut last = LastPushed::default();
                loop {
                    if let Err(e) = push_updates(&player, &shared, &mut last).await {
                        eprintln!("[nixic] MPRIS update failed: {e}");
                    }
                    tokio::time::sleep(PUSH_INTERVAL).await;
                }
            });
        })
        .ok();
}

/// Wire up D-Bus method calls to the app's command channel.
fn connect(player: &Player, tx: Arc<Sender<MprisCmd>>) {
    let c = tx.clone();
    player.connect_play(move |_| {
        let _ = c.send(MprisCmd::Play);
    });
    let c = tx.clone();
    player.connect_pause(move |_| {
        let _ = c.send(MprisCmd::Pause);
    });
    let c = tx.clone();
    player.connect_play_pause(move |_| {
        let _ = c.send(MprisCmd::PlayPause);
    });
    let c = tx.clone();
    player.connect_stop(move |_| {
        let _ = c.send(MprisCmd::Stop);
    });
    let c = tx.clone();
    player.connect_next(move |_| {
        let _ = c.send(MprisCmd::Next);
    });
    let c = tx.clone();
    player.connect_previous(move |_| {
        let _ = c.send(MprisCmd::Previous);
    });
    let c = tx.clone();
    player.connect_seek(move |_, t: Time| {
        let _ = c.send(MprisCmd::Seek(t.as_micros() as f64 / 1e6));
    });
    let c = tx.clone();
    player.connect_set_position(move |_, _id: &TrackId, t: Time| {
        let _ = c.send(MprisCmd::SetPosition(t.as_micros() as f64 / 1e6));
    });
    let c = tx.clone();
    player.connect_open_uri(move |_, uri: &str| {
        let _ = c.send(MprisCmd::OpenUri(uri.to_string()));
    });
    let c = tx.clone();
    player.connect_set_volume(move |_, v: Volume| {
        let _ = c.send(MprisCmd::SetVolume(v.clamp(0.0, 1.0)));
    });
    let c = tx.clone();
    player.connect_set_loop_status(move |_, ls: LoopStatus| {
        let code = match ls {
            LoopStatus::None => 0,
            LoopStatus::Playlist => 1,
            LoopStatus::Track => 2,
        };
        let _ = c.send(MprisCmd::SetLoop(code));
    });
    let c = tx.clone();
    player.connect_set_shuffle(move |_, shuffle: bool| {
        let _ = c.send(MprisCmd::SetShuffle(shuffle));
    });
    player.connect_quit(move |_| {
        let _ = tx.send(MprisCmd::Quit);
    });
}

/// What the server last advertised; used to avoid spamming property changes.
struct LastPushed {
    status: PlaybackStatus,
    track_id: String,
    title: String,
    artist: String,
    art_url: Option<String>,
    url: Option<String>,
    length_micros: Option<i64>,
    volume: Volume,
    loop_status: LoopStatus,
    shuffle: bool,
}

impl Default for LastPushed {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            track_id: String::new(),
            title: String::new(),
            artist: String::new(),
            art_url: None,
            url: None,
            length_micros: None,
            volume: 0.0,
            loop_status: LoopStatus::None,
            shuffle: false,
        }
    }
}

fn loop_from(raw: u8) -> LoopStatus {
    match raw {
        2 => LoopStatus::Track,
        1 => LoopStatus::Playlist,
        _ => LoopStatus::None,
    }
}

fn status_from(raw: u8) -> PlaybackStatus {
    match raw {
        1 => PlaybackStatus::Playing,
        2 => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

/// Diff the fresh snapshot against what we advertised and emit updates.
async fn push_updates(
    player: &Player,
    shared: &Arc<Mutex<Shared>>,
    last: &mut LastPushed,
) -> zbus::Result<()> {
    let (snap, seeked) = {
        let mut s = shared.lock().unwrap();
        (s.snap.clone(), s.seeked_micros.take())
    };

    // Seeked first so clients update their position promptly.
    if let Some(micros) = seeked {
        player.seeked(Time::from_micros(micros)).await?;
    }

    let status = status_from(snap.status);
    if status != last.status {
        player.set_playback_status(status).await?;
        last.status = status;
    }

    let meta_changed = snap.track_id != last.track_id
        || snap.title != last.title
        || snap.artist != last.artist
        || snap.art_url != last.art_url
        || snap.url != last.url
        || snap.length_micros != last.length_micros;
    if meta_changed {
        let mut b = Metadata::builder();
        if !snap.track_id.is_empty() {
            if let Ok(id) = TrackId::try_from(snap.track_id.as_str()) {
                b = b.trackid(id);
            }
        }
        if !snap.title.is_empty() {
            b = b.title(&snap.title);
        }
        if !snap.artist.is_empty() {
            b = b.artist([snap.artist.as_str()]);
        }
        if let Some(u) = &snap.art_url {
            b = b.art_url(u.clone());
        }
        if let Some(u) = &snap.url {
            b = b.url(u.clone());
        }
        if let Some(l) = snap.length_micros {
            b = b.length(Time::from_micros(l));
        }
        player.set_metadata(b.build()).await?;
        last.track_id = snap.track_id;
        last.title = snap.title;
        last.artist = snap.artist;
        last.art_url = snap.art_url;
        last.url = snap.url;
        last.length_micros = snap.length_micros;
    }

    if (snap.volume - last.volume).abs() > 0.001 {
        player.set_volume(snap.volume).await?;
        last.volume = snap.volume;
    }

    let loop_status = loop_from(snap.loop_status);
    if loop_status != last.loop_status {
        player.set_loop_status(loop_status).await?;
        last.loop_status = loop_status;
    }
    if snap.shuffle != last.shuffle {
        player.set_shuffle(snap.shuffle).await?;
        last.shuffle = snap.shuffle;
    }

    // Position never emits a signal (clients poll it); cheap to always set.
    player.set_position(Time::from_micros(snap.position_micros));
    Ok(())
}

/// Build a valid D-Bus object path for a track (video ids are sanitized).
pub fn track_path(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let safe = if safe.is_empty() { "track".to_string() } else { safe };
    format!("/nixic/{safe}")
}

/// Pull a YouTube video id out of a watch URL, a short link, or a bare id.
pub fn extract_video_id(uri: &str) -> Option<String> {
    if let Some(pos) = uri.find("v=") {
        let id = uri[pos + 2..].split(['&', '#']).next().unwrap_or("");
        if valid_id(id) {
            return Some(id.to_string());
        }
    }
    if let Some(pos) = uri.find("youtu.be/") {
        let id = uri[pos + 9..].split(['?', '&', '#']).next().unwrap_or("");
        if valid_id(id) {
            return Some(id.to_string());
        }
    }
    if valid_id(uri) {
        return Some(uri.to_string());
    }
    None
}

fn valid_id(id: &str) -> bool {
    id.len() >= 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_video_ids() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RD"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_video_id("https://youtu.be/dQw4w9WgXcQ?t=10"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(extract_video_id("dQw4w9WgXcQ"), Some("dQw4w9WgXcQ".to_string()));
        assert_eq!(extract_video_id("https://example.com/not-youtube"), None);
        assert_eq!(extract_video_id("short"), None);
    }

    #[test]
    fn track_paths_are_valid_and_unique() {
        let a = track_path("dQw4w9WgXcQ");
        let b = track_path("other-id-123");
        assert!(a.starts_with("/nixic/"));
        assert_ne!(a, b);
        // Sanitizes characters that are invalid in a D-Bus object path.
        assert!(track_path("weird id!!").contains("nixic/weird_id__"));
    }

    #[test]
    fn status_mapping() {
        assert_eq!(status_from(0), PlaybackStatus::Stopped);
        assert_eq!(status_from(1), PlaybackStatus::Playing);
        assert_eq!(status_from(2), PlaybackStatus::Paused);
        assert_eq!(status_from(99), PlaybackStatus::Stopped);
    }

    #[test]
    fn snapshot_diff_detects_metadata_change() {
        let mut last = LastPushed::default();
        let mut snap = Snapshot {
            status: 1,
            title: "Creep".into(),
            artist: "Radiohead".into(),
            track_id: "/nixic/9RfVp-GhKfs".into(),
            ..Default::default()
        };
        let changed = |last: &LastPushed, snap: &Snapshot| {
            snap.track_id != last.track_id
                || snap.title != last.title
                || snap.artist != last.artist
                || snap.art_url != last.art_url
                || snap.url != last.url
                || snap.length_micros != last.length_micros
        };
        assert!(changed(&last, &snap));
        last.track_id = snap.track_id.clone();
        last.title = snap.title.clone();
        last.artist = snap.artist.clone();
        assert!(!changed(&last, &snap));
        snap.title = "Creep (Acoustic)".into();
        assert!(changed(&last, &snap));
    }
}
