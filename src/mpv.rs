//! Audio playback through an `mpv` child process.
//!
//! We spawn mpv once in idle mode with an IPC socket, then drive it with
//! newline-delimited JSON commands and observe properties (position,
//! duration, pause, volume) via events pushed to a reader thread. Using mpv
//! as the engine means streaming, buffering, seeking and format support are
//! all handled for us.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum MpvEvent {
    Position(f64),
    Duration(f64),
    Pause(bool),
    Volume(f64),
    FileLoaded,
    EndFile { reason: String },
}

pub struct Mpv {
    child: Child,
    stream: UnixStream,
    events: Receiver<MpvEvent>,
}

impl Mpv {
    pub fn spawn(mpv_bin: &str, volume: u8) -> Result<Self> {
        let socket_path = std::env::temp_dir().join(format!(
            "nixic-mpv-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);

        let log_path = std::env::temp_dir().join("nixic-mpv.log");
        let log = File::create(&log_path).context("create mpv log file")?;
        let mut cmd = Command::new(mpv_bin);
        cmd.args([
            "--idle=yes",
            "--no-video",
            "--really-quiet",
            "--no-terminal",
            // When we hand mpv a watch URL directly (fallback path), mpv's
            // built-in yt-dlp integration resolves it; make sure it picks an
            // audio-only format.
            "--ytdl-format=bestaudio/best",
            // Stream cache + larger demuxer buffers so transient network
            // hiccups don't cause mid-stream stalls.
            "--cache=yes",
            "--demuxer-max-bytes=50M",
            "--demuxer-max-back-bytes=50M",
            &format!("--input-ipc-server={}", socket_path.display()),
            &format!("--volume={}", volume),
        ]);
        // Extra args from NIXIC_MPV_ARGS (e.g. "--ao=null" for headless testing).
        if let Ok(extra) = std::env::var("NIXIC_MPV_ARGS") {
            for arg in extra.split_whitespace() {
                cmd.arg(arg);
            }
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("failed to start {mpv_bin}"))?;

        // The socket appears shortly after startup; retry for a few seconds.
        let deadline = Instant::now() + Duration::from_secs(3);
        let stream = loop {
            match UnixStream::connect(&socket_path) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(40));
                }
                Err(e) => bail!("could not connect to mpv IPC socket: {e}"),
            }
        };

        let (tx, rx) = channel();
        let reader = stream.try_clone().context("clone mpv socket")?;
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // mpv closed the socket
                    Ok(_) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            if let Some(ev) = parse_event(&v) {
                                if tx.send(ev).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            child,
            stream,
            events: rx,
        })
    }

    /// Subscribe to the properties the UI needs.
    pub fn observe_all(&mut self) -> Result<()> {
        for (id, name) in [(1, "time-pos"), (2, "duration"), (3, "pause"), (4, "volume")] {
            self.raw(json!(["observe_property", id, name]))?;
        }
        Ok(())
    }

    /// Non-blocking poll for the next event. `Ok(None)` = nothing new,
    /// `Err` = the mpv event channel closed (mpv died).
    pub fn try_event(&self) -> Result<Option<MpvEvent>> {
        match self.events.try_recv() {
            Ok(ev) => Ok(Some(ev)),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(_) => bail!("mpv disconnected"),
        }
    }

    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Load a *direct* stream URL (e.g. a resolved googlevideo URL). yt-dlp
    /// resolution is disabled for it — otherwise mpv's ytdl hook re-fetches
    /// the URL as a generic video and YouTube's CDN answers 403, which made
    /// playback fail even though the URL was valid.
    pub fn load_direct(&mut self, url: &str) -> Result<()> {
        // Set per-load and never restore afterwards: mpv reads `ytdl` when it
        // opens the file, so restoring it right away raced with the open and
        // made mpv run yt-dlp on the direct URL again (CDN 403).
        self.set_ytdl(false)?;
        self.raw(json!(["loadfile", url, "replace"]))
    }

    /// Load a YouTube *watch* URL and let mpv's built-in yt-dlp integration
    /// resolve + play it (the fallback path when our own resolution fails).
    pub fn load_watch(&mut self, url: &str) -> Result<()> {
        self.set_ytdl(true)?;
        self.raw(json!(["loadfile", url, "replace"]))
    }

    fn set_ytdl(&mut self, on: bool) -> Result<()> {
        self.raw(json!(["set_property", "ytdl", on]))
    }

    // NOTE: mpv >= 0.41 rejects the bare `set` command when the value is a
    // native JSON type ("invalid parameter") — only `set_property` accepts
    // native values. All property writes must go through `set_property`.
    pub fn set_pause(&mut self, paused: bool) -> Result<()> {
        self.raw(json!(["set_property", "pause", paused]))
    }

    pub fn seek(&mut self, secs: f64) -> Result<()> {
        self.raw(json!(["seek", secs, "absolute"]))
    }

    pub fn set_volume(&mut self, vol: u8) -> Result<()> {
        self.raw(json!(["set_property", "volume", vol]))
    }

    pub fn stop(&mut self) -> Result<()> {
        self.raw(json!(["stop"]))
    }

    /// Enable/disable repeat-one: mpv loops the current file itself (so no
    /// `end-file` fires and the app never sees a track change).
    pub fn set_loop(&mut self, on: bool) -> Result<()> {
        let v = if on { "inf" } else { "no" };
        self.raw(json!(["set_property", "loop-file", v]))
    }

    fn raw(&mut self, command: Value) -> Result<()> {
        let mut line = serde_json::to_string(&json!({ "command": command }))?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        Ok(())
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Deliberately do NOT remove the socket file: when the app respawns
        // mpv after a crash, `self.mpv = new_mpv` drops the old instance and
        // this Drop would unlink the *new* instance's socket path (same path
        // every respawn). `Mpv::spawn` removes any stale file before
        // starting, so cleanup happens there instead.
    }
}

fn parse_event(v: &Value) -> Option<MpvEvent> {
    let event = v.get("event")?.as_str()?;
    match event {
        "property-change" => {
            let name = v.get("name")?.as_str()?;
            let data = v.get("data");
            match name {
                "time-pos" => data.and_then(|d| d.as_f64()).map(MpvEvent::Position),
                "duration" => data.and_then(|d| d.as_f64()).map(MpvEvent::Duration),
                "pause" => data.and_then(|d| d.as_bool()).map(MpvEvent::Pause),
                "volume" => data.and_then(|d| d.as_f64()).map(MpvEvent::Volume),
                _ => None,
            }
        }
        "end-file" => {
            let reason = v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            Some(MpvEvent::EndFile {
                reason: reason.to_string(),
            })
        }
        "file-loaded" => Some(MpvEvent::FileLoaded),
        _ => None,
    }
}
