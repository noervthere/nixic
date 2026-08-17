mod app;
mod cava;
mod mpv;
mod mpris;
mod music;
mod theme;
mod track;
mod ui;

use ratatui_image::picker::Picker;

use anyhow::{bail, Result};
use app::App;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use std::io::stdout;
use std::process::{Command, Stdio};
use std::time::Duration;

const DEFAULT_MPV: &str = "mpv";
const DEFAULT_YTDLP: &str = "yt-dlp";
const DEFAULT_VOLUME: u8 = 70;

/// Resolve a binary name from the environment (or default) and verify it
/// exists so we can fail with a helpful message before entering the TUI.
fn require_bin(env: &str, default: &str, pkg: &str) -> Result<String> {
    let bin = std::env::var(env).unwrap_or_else(|_| default.to_string());
    let found = Command::new(&bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        bail!(
            "{bin} not found in PATH. Install it, e.g.: nix profile install nixpkgs#{pkg}"
        );
    }
    Ok(bin)
}

/// Best-effort guess that the terminal implements a graphics protocol (or at
/// least answers DSR queries), based on env vars. Kitty/foot/wezterm/ghostty/
/// iterm set program-specific variables; VTE/Konsole-based terminals set
/// version vars. Anything else falls back to halfblocks, which works
/// everywhere.
fn terminal_likely_supports_graphics() -> bool {
    if std::env::var_os("TERM").is_none() {
        return false;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    if term == "dumb" || term.is_empty() {
        return false;
    }
    for kw in [
        "kitty", "foot", "wezterm", "ghostty", "contour", "mlterm", "sixel", "alacritty",
        "st", "xterm-kitty",
    ] {
        if term.contains(kw) {
            return true;
        }
    }
    for var in [
        "TERM_PROGRAM",
        "KITTY_WINDOW_ID",
        "WEZTERM_PANE",
        "GHOSTTY_RESOURCES_DIR",
        "ITERM_SESSION_ID",
        "KONSOLE_VERSION",
        "VTE_VERSION",
    ] {
        if std::env::var_os(var).is_some() {
            return true;
        }
    }
    false
}

fn main() -> Result<()> {
    let mpv_bin = require_bin("NIXIC_MPV_BIN", DEFAULT_MPV, "mpv")?;
    let ytdlp_bin = require_bin("NIXIC_YTDLP_BIN", DEFAULT_YTDLP, "yt-dlp")?;

    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        execute!(stdout(), event::EnableMouseCapture)?;
        // Load the theme config (hot-reloadable; also checked every 2s).
        if let Err(e) = theme::reload() {
            eprintln!("[nixic] theme: {e}");
        }
        // Detect the terminal's graphics protocol + font size for album art.
        // Must run before the first event poll (it reads the terminal's
        // answer from stdin). We only query when the terminal looks capable:
        // in a terminal that never answers (raw ptys, dumb terms) the query
        // thread would stay blocked on stdin and eat the first keystroke.
        let picker = if terminal_likely_supports_graphics() {
            match Picker::from_query_stdio() {
                Ok(p) => p,
                Err(_) => Picker::halfblocks(),
            }
        } else {
            Picker::halfblocks()
        };
        let mut app = App::new(&mpv_bin, &ytdlp_bin, DEFAULT_VOLUME, picker)?;
        loop {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            app.tick();
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => app.handle_key(k),
                    Event::Mouse(m) => app.handle_mouse(m),
                    _ => {}
                }
            }
            if app.should_quit {
                break;
            }
        }
        Ok(())
    })();
    let _ = execute!(stdout(), event::DisableMouseCapture);
    ratatui::restore();
    result
}
