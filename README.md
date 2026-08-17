# nixic 🎵

A TUI music player for **NixOS / Nix** with **YouTube Music** support. Search and
play music from YouTube Music right in your terminal, with full keyboard
**and** mouse support, real album art, a live spectrum visualizer, and
deep desktop integration via MPRIS.

![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Rust edition: 2021](https://img.shields.io/badge/rust-2021-orange.svg)
![Platform: Linux](https://img.shields.io/badge/platform-Linux-lightgrey.svg)
![Nix flake](https://img.shields.io/badge/nix-flake-5277C3.svg)

---

## Features

- **Structured dashboard layout** — a top navigation bar (Home / Albums /
  Artists / Playlists / Search), a now-playing player bar with playback
  status, centered title + artist, and one-click toggles (repeat, shuffle,
  mute, visualizer), a two-column content grid (track list with table headers
  on the left, large cover art + live spectrum analyzer on the right), and a
  bordered full-width progress **and** volume slider at the bottom.
- **Album art** — a large cover panel renders the track's real high-res
  thumbnail (480×360) via [ratatui-image](https://github.com/ratatui/ratatui-image):
  crisp sixel / kitty / iTerm2 graphics where your terminal supports them, and
  truecolor half-blocks everywhere else. The stateful image widget re-encodes
  itself whenever the panel is resized, so the cover stays sharp at any
  terminal size.
- **Theme follows your terminal** — nixic's palette is built from your
  terminal's ANSI scheme (so pywal / base16 / … changes apply instantly), and
  every role can be overridden in a config file that **hot-reloads**: press
  `Ctrl-r` or just edit the file — it's picked up automatically.
- **Repeat & shuffle** — `r` cycles repeat off → all → one, `s` toggles
  shuffle; both are reflected in MPRIS (`LoopStatus` / `Shuffle`) and shown as
  toggles in the player bar.
- **Real spectrum visualizer** — a full-width analyzer driven by
  [`cava`](https://github.com/karlstav/cava), reading your system audio and
  rendering smooth half-block bars with a color gradient. Toggle it with `v`;
  it degrades gracefully (flat bars / hidden) if cava isn't installed.
- **Reliable playback** — separate worker threads for search and resolve (so a
  slow search can never delay starting a song), timeouts + hard watchdogs on
  every yt-dlp call, a stall detector, and auto-restart if mpv crashes. When a
  direct stream fails mid-song (e.g. a transient CDN 403), nixic re-resolves
  with a **fresh URL** up to 3 times and **resumes from where the song cut
  off** instead of restarting it; if mpv itself dies it is respawned and
  playback continues. Broken tracks are skipped automatically.
- **Real artist names** — music-search results are enriched from matching
  plain-YouTube results (title matching) plus authoritative yt-dlp metadata,
  so the list shows `Creep — Radiohead`, not "Unknown Artist".
- **MPRIS** — nixic registers `org.mpris.MediaPlayer2.nixic` on the D-Bus
  session bus, so desktop environments (GNOME, KDE, …) show the current track
  (metadata + album art) and control it with media keys / widgets: play,
  pause, next, previous, stop, seek, volume, repeat, shuffle and even
  `OpenUri` (paste a YouTube link into your media widget).
- YouTube Music **and** plain YouTube search merged into one result list, so
  songs missing from the YT Music catalog still show up.

---

## Requirements

| Dependency | Needed for        | Required |
|------------|-------------------|----------|
| [`mpv`](https://mpv.io/) | audio engine — spawned in idle mode, driven over JSON IPC | ✅ |
| [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) | YouTube Music backend: search + resolving direct stream URLs | ✅ |
| [`cava`](https://github.com/karlstav/cava) | real-time spectrum visualizer | ⭕ optional |
| audio server (PipeWire / PulseAudio) | playback | ✅ |

No `ffmpeg` needed — nixic asks yt-dlp for a single `bestaudio` stream and mpv
plays it directly. For crisp sixel/kitty album art use a supporting terminal
(foot, kitty, wezterm, ghostty, iTerm2, …); everything else gets half-block
art automatically.

---

## Installation

### Nix (recommended)

The repository is a [Nix flake](https://nixos.wiki/wiki/Flakes) that bundles
**mpv, yt-dlp and cava** — no other setup needed. Works on `x86_64-linux` and
`aarch64-linux`.

```bash
# run directly from a checkout
nix run .

# install into your profile (wrapped with mpv / yt-dlp / cava)
nix profile install .
```

Or add it as a flake input and install declaratively:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nixic.url = "github:<you>/nixic";
  };
}
```

### From source

Requires a recent stable Rust toolchain plus the runtime dependencies above.

```bash
cargo run --release    # run without installing
cargo install --path . # installs nixic to ~/.cargo/bin
```

> ⚠️ `cargo install` does **not** bundle mpv / yt-dlp / cava — make sure they
> are on your `PATH` (e.g. `nix profile install nixpkgs#mpv nixpkgs#yt-dlp nixpkgs#cava`
> or your distro's packages).

### Development shell

```bash
nix develop   # cargo, rustc, clippy, rustfmt, rust-analyzer + mpv, yt-dlp, cava
```

---

## Environment variables

All optional:

| Variable          | Default  | Purpose                              |
|-------------------|----------|--------------------------------------|
| `NIXIC_MPV_BIN`   | `mpv`    | mpv binary path                      |
| `NIXIC_YTDLP_BIN` | `yt-dlp` | yt-dlp binary path                   |
| `NIXIC_MPV_ARGS`  | *(none)* | extra mpv args (e.g. `--ao=null`)    |
| `NIXIC_MPRIS`     | `1`      | set `0` to disable MPRIS             |

---

## Theme

Colors are stored in `~/.config/nixic/theme.toml` (or `$XDG_CONFIG_HOME`).
All values optional — missing roles follow your terminal's scheme. Values can
be ANSI names (`red`, `brightmagenta`, `gray`), `indexed(N)`, or `#rrggbb`
(RGB also re-enables the progress-bar gradient).

```toml
accent     = "red"        # highlights, buttons, active tab
accent_dim = "brightblack"
border     = "brightblack"
dim        = "brightblack"
play       = "green"      # progress bar fill
play_end   = "cyan"       # progress bar gradient end
```

Press `Ctrl-r` to reload, or just save the file — nixic picks it up on its
own within ~2 seconds.

---

## Controls

### Keyboard

| Key            | Action                          |
|----------------|---------------------------------|
| `/`            | search YouTube Music            |
| `Enter`        | play selected track (or search) |
| `Space`        | play / pause                    |
| `n` / `b`      | next / previous track           |
| `r`            | repeat: off / all / one         |
| `s`            | shuffle on / off                |
| `m`            | mute / unmute                   |
| `v`            | visualizer on / off             |
| `+` / `-`      | volume up / down                |
| `d`            | remove selected from queue      |
| `c`            | clear queue                     |
| `Ctrl-r`       | reload theme from config        |
| `h` / `?`      | help                            |
| `q`            | quit                            |

### Mouse

| Mouse            | Action              |
|------------------|---------------------|
| click row        | select track        |
| double-click     | play track          |
| scroll wheel     | navigate list       |
| click progress   | seek                |
| click volume     | set volume          |
| click status     | play / pause        |
| nav tabs         | switch views        |
| player toggles   | repeat / shuffle / mute / visualizer |

---

## Troubleshooting

- **No album art / blank cover** — your terminal needs sixel, kitty or iTerm2
  graphics support (foot, kitty, wezterm, ghostty, iTerm2, …). Otherwise nixic
  falls back to truecolor half-blocks automatically.
- **Visualizer stays flat or hidden** — cava isn't installed, isn't on
  `PATH`, or no audio server is running. Install it (or use the flake, which
  bundles it) and toggle with `v`.
- **Nothing plays** — sanity-check the backend manually:
  `yt-dlp -f bestaudio "https://www.youtube.com/watch?v=dQw4w9WgXcQ"`.
  Use `NIXIC_YTDLP_BIN` / `NIXIC_MPV_BIN` if your binaries live elsewhere.
- **Transient stream errors (CDN 403, stalls)** — nixic handles these
  automatically: it re-resolves a fresh URL up to 3 times and resumes from the
  cut-off position. If it keeps happening, your network may be throttling.
- **No MPRIS integration** — make sure you're on a D-Bus session bus and
  `NIXIC_MPRIS` isn't set to `0`. nixic registers
  `org.mpris.MediaPlayer2.nixic`.
- **Test with no audio device** — `NIXIC_MPV_ARGS=--ao=null nixic` lets you
  run the TUI without playing sound.

---

## Architecture

- `src/main.rs` — event loop (draw → tick → poll input), graphics-protocol detection
- `src/app.rs` — application state, repeat/shuffle, retry & watchdog logic, MPRIS wiring
- `src/ui.rs` — ratatui rendering, clickable zones, album-art widget
- `src/theme.rs` — terminal-scheme palette, config parsing, hot reload
- `src/mpv.rs` — mpv child process + JSON IPC client
- `src/music.rs` — yt-dlp workers (merged search + artist enrichment, timed-out resolution, album-art fetcher)
- `src/mpris.rs` — MPRIS D-Bus server (background thread, state diffing, command channel)
- `src/cava.rs` — cava subprocess, config generation, spectrum resampling
- `src/track.rs` — track model, formatting helpers

---

## Roadmap

- [ ] SoundCloud support
- [ ] Album / artist / playlist browsing views
- [x] Nix flake (bundles mpv / yt-dlp / cava)

---

## Contributing

Bug reports, feature ideas and pull requests are welcome! Please:

1. Keep changes small and focused.
2. Run `cargo fmt` and `cargo clippy` before opening a PR.
3. Add/update tests where behavior changes (`cargo test`).

---

## License

MIT — see [LICENSE](LICENSE).
