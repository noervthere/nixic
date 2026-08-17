//! Rendering. The layout is a structured dashboard:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ Home │ Albums │ Artists │ Playlists │ Search ────────│  top nav
//! ├──────────────────────────────────────────────────────┤
//! │ ▶ Playing • 0:56 / 1:26   Title       🔁 🔀 🔊 🎚  │  player bar
//! │                  Artist • ♪                          │
//! ├───────────────────────────┬──────────────────────────┤
//! │ ♪  ARTIST     ♫  TRACK   │  ┌──────────────────┐    │
//! │ ›  Radiohead     Creep    │  │                  │    │  content
//! │ …                         │  │    album art     │    │
//! │                           │  │                  │    │
//! │                           │  └──────────────────┘    │
//! ├──────────────────────────────────────────────────────┤
//! │ █▄██ █ ▄█ ██▄ █ ▄▄ █▄ ██ █ ██ █ ▄█ ██▄ █ ▄▄ █▄ ██│  visualizer
//! ├──────────────────────────────────────────────────────┤
//! │ ━━━━━━━━━━───── 0:56 / 1:26   🔊 ██░░ 70%          │  bottom bar
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! Every interactive region (tabs, toggles, list, progress, volume) is
//! recorded in `Zone`s so mouse events can be hit-tested against the last
//! drawn frame. All colors come from `theme::current()`, which follows the
//! terminal scheme and hot-reloads — this module never hardcodes a color.

use crate::app::{Action, App, Mode, RepeatMode, Zone, ZoneKind};
use crate::cava;
use crate::theme::{self, Theme};
use crate::track::{fmt_duration, fmt_time, Track};
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use ratatui_image::StatefulImage;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Height of the full-width visualizer strip.
const VIZ_H: u16 = 5;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let t = theme::current();
    let area = frame.area();

    // Build vertical layout: nav, player bar, content, [visualizer], bottom bar.
    let mut constraints = vec![
        Constraint::Length(1), // top navigation
        Constraint::Length(3), // now-playing player bar
        Constraint::Min(1),    // main content (track list + art)
    ];
    if app.viz_on {
        constraints.push(Constraint::Length(VIZ_H)); // full-width visualizer
    }
    constraints.push(Constraint::Length(3)); // bottom progress/volume bar

    let chunks = Layout::vertical(constraints).split(area);

    let mut zones = Vec::new();
    draw_nav(frame, chunks[0], app, &t, &mut zones);
    draw_player_bar(frame, chunks[1], app, &t, &mut zones);

    // Content: left column = track list, right column = album art.
    let content = Layout::horizontal([
        Constraint::Percentage(55),
        Constraint::Percentage(45),
    ])
    .split(chunks[2]);
    let (queue_h, search_h) = draw_content(frame, content[0], app, &t, &mut zones);
    draw_art_panel(frame, content[1], app, &t);

    // Full-width visualizer strip (if enabled).
    let bottom_idx = if app.viz_on {
        draw_visualizer(frame, chunks[3], app, &t);
        4
    } else {
        3
    };

    draw_bottom_bar(frame, chunks[bottom_idx], app, &t, &mut zones);
    app.zones = zones;

    app.update_scroll(queue_h, search_h);
}

// ----- top navigation -----

fn draw_nav(frame: &mut Frame, area: Rect, app: &App, t: &Theme, zones: &mut Vec<Zone>) {
    let tabs: &[(&str, Action, Mode)] = &[
        ("Home", Action::ShowHome, Mode::Home),
        ("Albums", Action::ShowAlbums, Mode::Albums),
        ("Artists", Action::ShowArtists, Mode::Artists),
        ("Playlists", Action::ShowPlaylists, Mode::Playlists),
        ("Search", Action::ShowSearch, Mode::Search),
    ];
    let mut x = area.x;
    for (i, (label, action, mode)) in tabs.iter().enumerate() {
        let active = app.mode == *mode;
        let text = format!(" {label} ");
        let w = text.width() as u16;
        let rect = Rect::new(x, area.y, w, 1);
        // Active tab: inverted/accented background block.
        let style = if active {
            Style::new()
                .fg(t.accent)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::new().fg(t.dim)
        };
        frame.render_widget(Paragraph::new(Span::styled(text, style)), rect);
        zones.push(Zone {
            kind: ZoneKind::Button(*action),
            rect,
        });
        x += w;
        if i < tabs.len() - 1 {
            frame.render_widget(
                Paragraph::new(Span::styled("│", Style::new().fg(t.border))),
                Rect::new(x, area.y, 1, 1),
            );
            x += 1;
        }
    }
    let fill_w = area.width.saturating_sub(x - area.x);
    if fill_w > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(fill_w as usize),
                Style::new().fg(t.border),
            ))),
            Rect::new(x, area.y, fill_w, 1),
        );
    }
}

// ----- player bar -----

fn draw_player_bar(frame: &mut Frame, area: Rect, app: &App, t: &Theme, zones: &mut Vec<Zone>) {
    let w = area.width as usize;
    let row0 = Rect::new(area.x, area.y, area.width, 1);
    let row1 = Rect::new(area.x, area.y + 1, area.width, 1);

    let track = app.current_track();
    // Left: playback status + timestamp + total duration.
    let (icon, status) = match (track.is_some(), app.playing) {
        (true, true) => ("▶", "Playing"),
        (true, false) => ("⏸", "Paused"),
        (false, _) => ("⏹", "Stopped"),
    };
    let elapsed = fmt_time(app.position);
    let total = app
        .duration
        .map(fmt_time)
        .unwrap_or_else(|| "--:--".to_string());
    let left = format!("{icon} {status} • {elapsed} / {total}");

    // Right: control toggles — repeat, shuffle, mute, visualizer.
    let mut toggles: Vec<(String, Style, Action)> = Vec::new();
    let (rep_text, rep_on) = match app.repeat {
        RepeatMode::Off => ("🔁".to_string(), false),
        RepeatMode::All => ("🔁 All".to_string(), true),
        RepeatMode::One => ("🔂".to_string(), true),
    };
    let on = |on: bool| {
        if on {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.dim)
        }
    };
    toggles.push((rep_text, on(rep_on), Action::CycleRepeat));
    toggles.push(("🔀".to_string(), on(app.shuffle), Action::ToggleShuffle));
    let vol_icon = if app.muted { "🔇" } else { "🔊" };
    toggles.push((vol_icon.to_string(), on(app.muted), Action::ToggleMute));
    toggles.push(("🎚".to_string(), on(app.viz_on), Action::ToggleVisualizer));

    let left_w = left.width();
    let right_w: usize = toggles.iter().map(|(s, _, _)| s.width()).sum::<usize>()
        + toggles.len().saturating_sub(1);
    let mid_w = w.saturating_sub(left_w + right_w + 2);
    let title = track
        .map(|tr| truncate(&tr.title, mid_w.saturating_sub(2)))
        .unwrap_or_default();
    let title_w = title.width();
    let lpad = (mid_w.saturating_sub(title_w)) / 2;
    let rpad = mid_w.saturating_sub(lpad + title_w);

    let mut spans = vec![
        Span::styled(left.clone(), Style::new().fg(t.dim)),
        Span::raw(" ".repeat(lpad + 1)),
        Span::styled(title, Style::new().white().bold()),
        Span::raw(" ".repeat(rpad + 1)),
    ];
    // Click the status region to play / pause.
    zones.push(Zone {
        kind: ZoneKind::Button(Action::TogglePlay),
        rect: Rect::new(area.x, area.y, left_w as u16, 1),
    });
    for (i, (text, style, _)) in toggles.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(text.clone(), *style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), row0);

    // Clickable zones for the toggles (right-aligned).
    let mut tx = area.x + area.width;
    let mut rev: Vec<&(String, Style, Action)> = toggles.iter().collect();
    rev.reverse();
    for (text, _style, action) in rev {
        let tw = text.width() as u16;
        tx = tx.saturating_sub(tw);
        zones.push(Zone {
            kind: ZoneKind::Button(*action),
            rect: Rect::new(tx, area.y, tw, 1),
        });
        tx = tx.saturating_sub(1);
    }

    // Subtext: artist • album (centered).
    let sub = match track {
        Some(tr) => format!("{} • ♪", tr.artist),
        None => "Nothing playing — press / or click [Search]".to_string(),
    };
    let sub = truncate(&sub, w.saturating_sub(2));
    let pad = (w.saturating_sub(sub.width())) / 2;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}{}", " ".repeat(pad), sub),
            Style::new().fg(t.dim),
        ))),
        row1,
    );

    // Divider under the player bar.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(w),
            Style::new().fg(t.border),
        ))),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );
}

// ----- content grid -----

fn draw_content(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    t: &Theme,
    zones: &mut Vec<Zone>,
) -> (usize, usize) {
    match app.mode {
        Mode::Home => (draw_queue(frame, area, app, t, zones), 0),
        Mode::Search => (0, draw_search(frame, area, app, t, zones)),
        Mode::Help => {
            draw_help(frame, area, t);
            (0, 0)
        }
        Mode::Albums | Mode::Artists | Mode::Playlists => {
            draw_placeholder(frame, area, app.mode, t);
            (0, 0)
        }
    }
}

fn draw_placeholder(frame: &mut Frame, area: Rect, mode: Mode, t: &Theme) {
    let name = match mode {
        Mode::Albums => "Albums",
        Mode::Artists => "Artists",
        _ => "Playlists",
    };
    let lines = vec![
        Line::from(Span::styled(
            format!("{name} browsing is coming soon"),
            Style::new().white().bold(),
        )),
        Line::from(Span::styled(
            "Press h or Esc to return Home",
            Style::new().fg(t.dim),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Build one list row with the table columns: selection indicator, artist,
/// music-note icon, track title, right-aligned status.
fn track_row(tr: &Track, is_current: bool, cursor: bool, list_w: usize, t: &Theme) -> Line<'static> {
    let sel = if is_current {
        "♪"
    } else if cursor {
        "›"
    } else {
        " "
    };
    let art_w = ((list_w * 34) / 100).max(8);
    let right_w = 8usize;
    let icon_w = 3usize; // " ♫ "
    let track_w = list_w.saturating_sub(2 + art_w + icon_w + right_w);
    let artist = truncate(&tr.artist, art_w);
    let title = truncate(&tr.title, track_w.max(1));
    let title_style = if is_current {
        Style::new().fg(t.accent).bold()
    } else {
        Style::new().white()
    };
    let pad = list_w.saturating_sub(2 + art_w + icon_w + title.width() + right_w);
    Line::from(vec![
        Span::styled(sel, Style::new().fg(t.accent).bold()),
        Span::raw(" "),
        Span::styled(
            format!("{artist:<art_w$}"),
            Style::new().fg(t.dim),
        ),
        Span::styled(" ♫ ", Style::new().fg(t.border)),
        Span::styled(title, title_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            format!("{:>8}", fmt_duration(tr.duration)),
            Style::new().fg(t.dim),
        ),
    ])
}

/// The table header row, using the same column widths as `track_row`.
fn track_header(list_w: usize, t: &Theme) -> Line<'static> {
    let art_w = ((list_w * 34) / 100).max(8);
    let right_w = 8usize;
    let icon_w = 3usize;
    let pad = list_w.saturating_sub(2 + art_w + icon_w + "TRACK".width() + right_w);
    Line::from(vec![
        Span::styled("♪", Style::new().fg(t.accent)),
        Span::raw(" "),
        Span::styled(format!("{:<art_w$}", "ARTIST"), Style::new().fg(t.dim).bold()),
        Span::styled(" ♫ ", Style::new().fg(t.border)),
        Span::styled("TRACK", Style::new().fg(t.dim).bold()),
        Span::raw(" ".repeat(pad)),
        Span::styled(format!("{:>8}", "▶"), Style::new().fg(t.dim).bold()),
    ])
}

fn draw_queue(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    t: &Theme,
    zones: &mut Vec<Zone>,
) -> usize {
    if app.queue.is_empty() {
        let empty = Paragraph::new(Line::from(vec![
            Span::styled("Queue is empty", Style::new().white().bold()),
            Span::raw("\n"),
            Span::styled(
                "Press / or click [Search] to find music on YouTube Music",
                Style::new().fg(t.dim),
            ),
        ]))
        .alignment(Alignment::Center);
        frame.render_widget(empty, area);
        return 0;
    }
    if area.height >= 2 {
        frame.render_widget(
            Paragraph::new(track_header(area.width as usize, t)),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    let list_area = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
    let list_w = area.width as usize;
    let items: Vec<ListItem> = app
        .queue
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            let is_current = app.current == Some(i);
            let cursor = app.queue_cursor == i;
            ListItem::new(track_row(tr, is_current, cursor, list_w, t))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().fg(t.accent).add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    *state.offset_mut() = app.queue_offset;
    state.select(Some(app.queue_cursor.min(app.queue.len().saturating_sub(1))));
    frame.render_stateful_widget(list, list_area, &mut state);
    zones.push(Zone {
        kind: ZoneKind::List,
        rect: list_area,
    });
    list_area.height as usize
}

fn draw_search(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    t: &Theme,
    zones: &mut Vec<Zone>,
) -> usize {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(area);

    // Input line
    let prompt = "Search: ";
    let mut spans = vec![Span::styled(prompt, Style::new().fg(t.accent).bold())];
    if app.search_input.is_empty() {
        spans.push(Span::styled(
            "type to search, Enter to run",
            Style::new().fg(t.dim),
        ));
    } else {
        spans.push(Span::styled(app.search_input.clone(), Style::new().white()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);
    let cursor_x = ((chunks[0].x as usize + prompt.width() + app.search_input.width())
        .min((chunks[0].x + chunks[0].width.saturating_sub(1)) as usize)) as u16;
    frame.set_cursor_position(Position::new(cursor_x, chunks[0].y));

    // Hint / status line
    let hint = if app.search_loading {
        Line::from(Span::styled(
            "Searching…",
            Style::new().fg(t.accent_dim).bold(),
        ))
    } else if let Some(err) = &app.search_error {
        Line::from(Span::styled(
            truncate(err, area.width as usize),
            Style::new().red(),
        ))
    } else if app.search_results.is_empty() {
        Line::from(Span::styled(
            "No results yet — press Enter to search",
            Style::new().fg(t.dim),
        ))
    } else {
        Line::from(Span::styled(
            format!(
                "{} results — ↑/↓ to select, Enter or double-click to play",
                app.search_results.len()
            ),
            Style::new().fg(t.dim),
        ))
    };
    frame.render_widget(Paragraph::new(hint), chunks[1]);

    if app.search_results.is_empty() {
        return 0;
    }
    let list_w = chunks[2].width as usize;
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            let cursor = app.search_cursor == i;
            ListItem::new(track_row(tr, false, cursor, list_w, t))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().fg(t.accent).add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    *state.offset_mut() = app.search_offset;
    state.select(Some(app.search_cursor.min(app.search_results.len().saturating_sub(1))));
    frame.render_stateful_widget(list, chunks[2], &mut state);
    zones.push(Zone {
        kind: ZoneKind::List,
        rect: chunks[2],
    });
    chunks[2].height as usize
}

fn draw_help(frame: &mut Frame, area: Rect, t: &Theme) {
    let mut lines = Vec::new();
    let header =
        |s: &str| Line::from(Span::styled(s.to_string(), Style::new().fg(t.accent).bold()));
    let row = |key: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {key:<12}"), Style::new().fg(t.play)),
            Span::raw(desc.to_string()),
        ])
    };
    lines.push(header("Keyboard"));
    for (k, d) in [
        ("/", "search YouTube Music"),
        ("Enter", "play selected track"),
        ("Space", "play / pause"),
        ("n / b", "next / previous track"),
        ("r", "repeat: off / all / one"),
        ("s", "shuffle on / off"),
        ("m", "mute / unmute"),
        ("v", "visualizer on / off"),
        ("+ / -", "volume up / down"),
        ("d", "remove selected from queue"),
        ("c", "clear queue"),
        ("Ctrl-r", "reload theme from config"),
        ("h / ?", "this help"),
        ("q", "quit"),
    ] {
        lines.push(row(k, d));
    }
    lines.push(Line::raw(""));
    lines.push(header("Mouse"));
    for (k, d) in [
        ("click row", "select track"),
        ("double-click", "play track"),
        ("scroll wheel", "navigate list"),
        ("progress bar", "click to seek"),
        ("volume slider", "click to set volume"),
        ("tabs", "switch views"),
        ("toggles", "repeat / shuffle / mute / visualizer"),
    ] {
        lines.push(row(k, d));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Press q or Esc to return",
        Style::new().fg(t.dim),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

// ----- side panel: album art -----

fn draw_art_panel(frame: &mut Frame, area: Rect, app: &mut App, t: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.border));
    frame.render_widget(block, area);
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    match &mut app.art_state {
        Some(state) => {
            let img = StatefulImage::default();
            frame.render_stateful_widget(img, inner, state);
        }
        None => {
            let style = Style::new().fg(t.border);
            let lines = vec![
                Line::raw(""),
                Line::from(Span::styled("♪", style)),
                Line::from(Span::styled("no art yet", style)),
                Line::raw(""),
            ];
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                inner,
            );
        }
    }
}

// ----- full-width visualizer strip (powered by cava) -----

fn draw_visualizer(frame: &mut Frame, area: Rect, app: &App, t: &Theme) {
    let block = Block::bordered()
        .title(" Visualizer ")
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.border));
    frame.render_widget(block, area);
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    if inner_w == 0 || inner_h == 0 {
        return;
    }

    // Read the latest cava bars and resample to terminal width.
    let raw_bars = app
        .cava
        .as_ref()
        .map(|c| c.bars())
        .unwrap_or_default();
    let bars = cava::resample(&raw_bars, inner_w);

    // Each terminal row represents 2 vertical sub-rows via half-blocks.
    // Total vertical resolution = inner_h * 2 sub-rows.
    let sub_rows = inner_h * 2;
    let mut lines = Vec::with_capacity(inner_h);
    for r in 0..inner_h {
        let mut spans = Vec::with_capacity(inner_w);
        for (i, &val) in bars.iter().enumerate() {
            // How many sub-rows this bar fills (from the bottom).
            let fill = (val * sub_rows as f64).round() as usize;
            // The two sub-rows represented by terminal row `r`:
            //   top_sub = r * 2      (top half of cell)
            //   bot_sub = r * 2 + 1  (bottom half of cell)
            // Sub-row 0 is the TOP of the visualizer.
            // A bar fills from the bottom: sub-rows [sub_rows - fill, sub_rows).
            let top_filled = sub_rows.saturating_sub(r * 2) <= fill;
            let bot_filled = sub_rows.saturating_sub(r * 2 + 1) <= fill;

            // Gradient color across the width.
            let bar_color = if inner_w > 1 {
                let frac = i as f64 / (inner_w - 1) as f64;
                lerp(t.play, t.play_end, frac)
            } else {
                t.play
            };

            let (ch, style) = match (top_filled, bot_filled) {
                (true, true) => ('█', Style::new().fg(bar_color)),
                (false, true) => ('▄', Style::new().fg(bar_color)),
                (true, false) => ('▀', Style::new().fg(bar_color)),
                (false, false) => (' ', Style::default()),
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x + 1, area.y + 1, inner_w as u16, inner_h as u16),
    );
}

// ----- bottom bar: progress + volume -----

fn draw_bottom_bar(frame: &mut Frame, area: Rect, app: &App, t: &Theme, zones: &mut Vec<Zone>) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.border));
    frame.render_widget(block, area);
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
    let w = inner.width as usize;
    if w == 0 {
        return;
    }

    let elapsed = fmt_time(app.position);
    let total = app
        .duration
        .map(fmt_time)
        .unwrap_or_else(|| "--:--".to_string());
    let time = format!(" {elapsed} / {total} ");
    let time_w = time.width();
    let vol_icon = if app.muted { "🔇 " } else { "🔊 " };
    let vol_icon_w = vol_icon.width();
    let vol_label = format!(" {}% ", app.volume);
    let vol_label_w = vol_label.width();

    let prog_w = ((w as f64) * 0.5) as usize;
    let vol_w = w.saturating_sub(prog_w + time_w + vol_icon_w + vol_label_w + 2);

    // Progress slider
    let frac = app
        .duration
        .filter(|d| *d > 0.0)
        .map(|d| (app.position / d).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let filled = ((prog_w as f64) * frac).round() as usize;
    let mut spans = gradient_progress(filled, prog_w, app.playing, t);
    zones.push(Zone {
        kind: ZoneKind::Progress,
        rect: Rect::new(inner.x, inner.y, prog_w as u16, 1),
    });

    spans.push(Span::styled(time, Style::new().fg(t.dim)));
    spans.push(Span::styled(vol_icon, Style::new().fg(t.dim)));

    // Volume slider
    let vol_filled = (app.volume as usize * vol_w) / 100;
    for i in 0..vol_w {
        let ch = if i < vol_filled { "█" } else { "░" };
        let style = if i < vol_filled {
            Style::new().fg(t.accent)
        } else {
            Style::new().fg(t.border)
        };
        spans.push(Span::styled(ch, style));
    }
    let vol_x = inner.x + prog_w as u16 + time_w as u16 + vol_icon_w as u16;
    zones.push(Zone {
        kind: ZoneKind::Volume,
        rect: Rect::new(vol_x, inner.y, vol_w as u16, 1),
    });
    spans.push(Span::styled(vol_label, Style::new().fg(t.dim)));

    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

// ----- helpers -----

/// Interpolate between two RGB colors; falls back to `a` for non-RGB inputs.
fn lerp(a: ratatui::style::Color, b: ratatui::style::Color, tt: f64) -> ratatui::style::Color {
    let (ratatui::style::Color::Rgb(r1, g1, b1), ratatui::style::Color::Rgb(r2, g2, b2)) = (a, b)
    else {
        return a;
    };
    let mix =
        |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * tt).round().clamp(0.0, 255.0) as u8;
    ratatui::style::Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
}

/// A progress bar where each filled cell fades from `play` to `play_end`.
/// With non-RGB (terminal-scheme) colors there is no gradient, so the bar is
/// a solid accent.
fn gradient_progress(filled: usize, total: usize, playing: bool, t: &Theme) -> Vec<Span<'static>> {
    let (play, play_end) = (t.play, t.play_end);
    let gradient = matches!(play, ratatui::style::Color::Rgb(..))
        && matches!(play_end, ratatui::style::Color::Rgb(..));
    let mut spans = Vec::with_capacity(total);
    for i in 0..total {
        if i < filled {
            let style = if playing {
                if gradient {
                    let tt = if total > 0 { i as f64 / total as f64 } else { 0.0 };
                    Style::new().fg(lerp(play, play_end, tt))
                } else {
                    Style::new().fg(play)
                }
            } else {
                Style::new().fg(t.dim)
            };
            spans.push(Span::styled("━", style));
        } else {
            spans.push(Span::styled("─", Style::new().fg(t.border)));
        }
    }
    spans
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_respect_column_widths() {
        let tr = Track {
            id: "dQw4w9WgXcQ".into(),
            title: "Creep".into(),
            artist: "Radiohead".into(),
            duration: Some(238.0),
            url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(),
        };
        let t = crate::theme::terminal_default();
        let line = track_row(&tr, true, false, 60, &t);
        let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
        assert!(total <= 60, "row overflows: {total} > 60");
    }

    #[test]
    fn cava_resample_identity() {
        let bars = vec![0.0, 0.5, 1.0];
        let out = cava::resample(&bars, 3);
        assert_eq!(out, bars);
    }

    #[test]
    fn cava_resample_upscale() {
        let bars = vec![0.0, 1.0];
        let out = cava::resample(&bars, 5);
        assert_eq!(out.len(), 5);
        assert!((out[0] - 0.0).abs() < 0.01);
        assert!((out[4] - 1.0).abs() < 0.01);
        assert!((out[2] - 0.5).abs() < 0.01);
    }

    #[test]
    fn cava_resample_single_column_is_not_nan() {
        // Degenerate width must never produce NaN (regression: divide-by-zero
        // in the interpolation scale).
        let out = cava::resample(&[0.3, 0.9], 1);
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_nan());
        assert!((out[0] - 0.3).abs() < 0.01);
        // Empty input on a single column is a flat zero.
        let out = cava::resample(&[], 1);
        assert_eq!(out, vec![0.0]);
    }
}
