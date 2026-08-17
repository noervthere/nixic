//! Core data model for tracks plus small formatting/encoding helpers.

#[derive(Debug, Clone)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    /// Duration in seconds, if known.
    pub duration: Option<f64>,
    /// Watch URL used to resolve a stream (may be a music.youtube.com URL).
    pub url: String,
}

/// Format seconds as `m:ss` or `h:mm:ss`.
pub fn fmt_time(secs: f64) -> String {
    let total = secs.max(0.0).floor() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_duration(opt: Option<f64>) -> String {
    opt.map(fmt_time).unwrap_or_else(|| "--:--".to_string())
}

/// Build thumbnail URLs for a YouTube video id in descending resolution order.
/// `maxresdefault` is 1280×720, `sddefault` is 640×480, `hqdefault` is 480×360.
/// Not all videos have maxres/sd variants — the fetcher tries each and falls back.
pub fn thumb_urls(video_id: &str) -> Vec<String> {
    if video_id.len() < 11 {
        return Vec::new();
    }
    ["maxresdefault", "sddefault", "hqdefault"]
        .iter()
        .map(|q| format!("https://i.ytimg.com/vi/{video_id}/{q}.jpg"))
        .collect()
}

/// Best single thumbnail URL (for MPRIS metadata — only needs one URL, not a
/// chain). Deliberately `hqdefault`: `maxresdefault`/`sddefault` 404 for many
/// videos, and a media widget has no fallback chain — `hqdefault` virtually
/// always exists.
pub fn thumb_url(video_id: &str) -> Option<String> {
    thumb_urls(video_id).into_iter().next_back()
}

/// Percent-encode a string for use in a query component (space -> %20, not +).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_emits_changed_cells() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::style::{Style, Stylize};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;
        use ratatui::Terminal;

        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let draw_row = |term: &mut Terminal<TestBackend>, pos: f64| {
            term.draw(|f| {
                let elapsed = fmt_time(pos);
                let right = format!(" {elapsed} / --:--  Vol 70% ");
                let bar = "─".repeat(76);
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(bar, Style::new().cyan()),
                        Span::styled(right, Style::new().dim()),
                    ])),
                    Rect::new(0, 1, 100, 1),
                );
            })
            .unwrap();
        };
        draw_row(&mut term, 0.0);
        let before = format!("{:?}", term.backend().buffer());
        assert!(before.contains("0:00"), "first frame: {before:?}");
        draw_row(&mut term, 1.04);
        let after = format!("{:?}", term.backend().buffer());
        assert!(after.contains("0:01"), "second frame did not update: {after:?}");
    }

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(59.9), "0:59");
        assert_eq!(fmt_time(65.0), "1:05");
        assert_eq!(fmt_time(3600.0), "1:00:00");
        assert_eq!(fmt_time(3661.5), "1:01:01");
    }

    #[test]
    fn url_encoding() {
        assert_eq!(urlencode("radiohead creep"), "radiohead%20creep");
        assert_eq!(urlencode("a&b/c"), "a%26b%2Fc");
        assert_eq!(urlencode("plain"), "plain");
    }
}
