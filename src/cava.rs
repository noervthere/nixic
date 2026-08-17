//! Real audio visualizer powered by `cava`.
//!
//! Cava is spawned as a child process with a generated config that outputs
//! raw ASCII to stdout: each frame is a line of semicolon-delimited bar
//! heights. A reader thread continuously parses frames and stores the latest
//! one behind an `Arc<Mutex<>>` so the UI can grab it lock-free on every tick.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Number of bars cava produces. We pick a moderate count and resample to
/// whatever the terminal width is at render time.
const CAVA_BARS: usize = 64;
/// Cava framerate — 60 fps is smooth and cheap in raw mode.
const CAVA_FPS: u32 = 60;
/// The ASCII max range: bar values are 0..=this.
const CAVA_RANGE: u32 = 1000;

pub struct CavaVisualizer {
    child: Option<Child>,
    latest: Arc<Mutex<Vec<f64>>>,
    /// Path to the temporary config file (cleaned up on drop).
    config_path: std::path::PathBuf,
}

impl CavaVisualizer {
    /// Spawn cava with a generated config. `cava_bin` is the path to the
    /// cava binary (usually just `"cava"`).
    pub fn spawn(cava_bin: &str) -> Result<Self, String> {
        let config_path = std::env::temp_dir().join(format!(
            "nixic-cava-{}.conf",
            std::process::id()
        ));
        let config = format!(
            "\
[general]
bars = {CAVA_BARS}
framerate = {CAVA_FPS}
autosens = 1
sensitivity = 100

[output]
method = raw
raw_target = /dev/stdout
data_format = ascii
ascii_max_range = {CAVA_RANGE}
bar_delimiter = 59
frame_delimiter = 10
"
        );
        std::fs::write(&config_path, config)
            .map_err(|e| format!("failed to write cava config: {e}"))?;

        let mut child = Command::new(cava_bin)
            .args(["-p", &config_path.to_string_lossy()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn cava: {e}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "cava stdout unavailable".to_string())?;

        let latest: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(vec![0.0; CAVA_BARS]));
        let reader_latest = Arc::clone(&latest);

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.is_empty() {
                    continue;
                }
                let bars: Vec<f64> = line
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse::<f64>().ok())
                    .map(|v| (v / CAVA_RANGE as f64).clamp(0.0, 1.0))
                    .collect();
                if bars.is_empty() {
                    continue;
                }
                if let Ok(mut guard) = reader_latest.lock() {
                    *guard = bars;
                }
            }
        });

        Ok(Self {
            child: Some(child),
            latest,
            config_path,
        })
    }

    /// Get the latest bar heights as normalized values in `[0.0, 1.0]`.
    /// Returns an empty vec if cava hasn't produced a frame yet.
    pub fn bars(&self) -> Vec<f64> {
        self.latest
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Check whether the cava process is still running.
    pub fn is_alive(&mut self) -> bool {
        self.child
            .as_mut()
            .map(|c| c.try_wait().ok().flatten().is_none())
            .unwrap_or(false)
    }

    /// Kill the cava process (idempotent).
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for CavaVisualizer {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_file(&self.config_path);
    }
}

/// Resample a cava bar array to a different column count using linear
/// interpolation. This maps `CAVA_BARS` bars to whatever the terminal
/// width happens to be.
pub fn resample(bars: &[f64], target_cols: usize) -> Vec<f64> {
    if bars.is_empty() || target_cols == 0 {
        return vec![0.0; target_cols];
    }
    // Guard against the degenerate 1-column case (avoids divide-by-zero in
    // the interpolation scale below, which would yield NaN).
    if target_cols == 1 {
        return vec![bars.first().copied().unwrap_or(0.0).clamp(0.0, 1.0)];
    }
    if bars.len() == target_cols {
        return bars.to_vec();
    }
    let mut out = Vec::with_capacity(target_cols);
    let scale = (bars.len() as f64 - 1.0) / (target_cols.max(1) as f64 - 1.0);
    for i in 0..target_cols {
        let pos = i as f64 * scale;
        let lo = pos.floor() as usize;
        let hi = (lo + 1).min(bars.len() - 1);
        let frac = pos - lo as f64;
        let val = bars[lo] * (1.0 - frac) + bars[hi] * frac;
        out.push(val.clamp(0.0, 1.0));
    }
    out
}
