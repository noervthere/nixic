//! Visual theme for nixic.
//!
//! The theme is derived from the **terminal's own color scheme**: roles map to
//! ANSI palette slots (`Color::Indexed`) and the default fg/bg, so when you
//! switch your terminal theme (pywal, base16, …) nixic follows automatically.
//!
//! Any role can be overridden with a config file, and nixic hot-reloads it:
//! press `Ctrl-r`, or just edit the file and it is picked up on its own
//! (checked every ~2 seconds). Colors may be ANSI names (`red`,
//! `brightmagenta`), `indexed(N)`, or hex `#rrggbb` — RGB overrides also
//! re-enable the progress-bar gradient.

use ratatui::style::Color;
use std::sync::RwLock;

/// One role in the palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub accent: Color,
    pub accent_dim: Color,
    pub border: Color,
    pub dim: Color,
    pub play: Color,
    pub play_end: Color,
}

/// ANSI-palette defaults: follows whatever scheme the terminal runs.
pub fn terminal_default() -> Theme {
    Theme {
        accent: Color::Indexed(1),
        accent_dim: Color::Indexed(8),
        border: Color::Indexed(8),
        dim: Color::Indexed(8),
        play: Color::Indexed(2),
        play_end: Color::Indexed(6),
    }
}

static THEME: RwLock<Theme> = RwLock::new(Theme {
    accent: Color::Indexed(1),
    accent_dim: Color::Indexed(8),
    border: Color::Indexed(8),
    dim: Color::Indexed(8),
    play: Color::Indexed(2),
    play_end: Color::Indexed(6),
});

/// Snapshot of the current theme (cheap: six colors).
pub fn current() -> Theme {
    match THEME.read() {
        Ok(g) => *g,
        Err(_) => terminal_default(),
    }
}

/// Re-read the config file (if any) and update the active theme. Returns a
/// human-readable error if the file exists but can't be parsed.
pub fn reload() -> Result<(), String> {
    let t = load_config()?;
    if let Ok(mut g) = THEME.write() {
        *g = t;
    }
    Ok(())
}

/// Path to the theme config file (`$XDG_CONFIG_HOME/nixic/theme.toml` or
/// `~/.config/nixic/theme.toml`), regardless of whether it exists yet.
pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from(".config"));
    base.join("nixic").join("theme.toml")
}

fn load_config() -> Result<Theme, String> {
    let path = config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(terminal_default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let value: toml::Value =
        toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("{}: expected a table", path.display()))?;

    let mut t = terminal_default();
    let mut bad = Vec::new();
    for (key, val) in table {
        let color = match val.as_str().map(parse_color) {
            Some(Some(c)) => c,
            _ => {
                bad.push(format!("{key}: expected a color string"));
                continue;
            }
        };
        match key.as_str() {
            "accent" => t.accent = color,
            "accent_dim" => t.accent_dim = color,
            "border" => t.border = color,
            "dim" => t.dim = color,
            "play" => t.play = color,
            "play_end" => t.play_end = color,
            _ => bad.push(format!("{key}: unknown key")),
        }
    }
    if !bad.is_empty() {
        return Err(format!("{}: {}", path.display(), bad.join("; ")));
    }
    Ok(t)
}

/// Parse a color value: ANSI name, `indexed(N)`, `N`, or `#rrggbb`.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    let lower = s.to_lowercase();
    let named = match lower.as_str() {
        "default" | "reset" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "brightblack" | "gray" | "grey" => Some(Color::DarkGray),
        "brightred" => Some(Color::LightRed),
        "brightgreen" => Some(Color::LightGreen),
        "brightyellow" => Some(Color::LightYellow),
        "brightblue" => Some(Color::LightBlue),
        "brightmagenta" => Some(Color::LightMagenta),
        "brightcyan" => Some(Color::LightCyan),
        "brightwhite" => Some(Color::Gray),
        _ => None,
    };
    if let Some(c) = named {
        return Some(c);
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
    }
    if let Some(inner) = lower.strip_prefix("indexed(").and_then(|x| x.strip_suffix(')')) {
        return inner.parse::<u8>().ok().map(Color::Indexed);
    }
    if let Ok(n) = lower.parse::<u8>() {
        return Some(Color::Indexed(n));
    }
    None
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_color_names() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("BrightMagenta"), Some(Color::LightMagenta));
        assert_eq!(parse_color("gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("default"), Some(Color::Reset));
        assert_eq!(parse_color("indexed(5)"), Some(Color::Indexed(5)));
        assert_eq!(parse_color("42"), Some(Color::Indexed(42)));
        assert_eq!(parse_color("#ff3b5f"), Some(Color::Rgb(255, 59, 95)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn defaults_follow_terminal_palette() {
        let t = terminal_default();
        assert_eq!(t.accent, Color::Indexed(1));
        assert_eq!(t.border, Color::Indexed(8));
    }
}
