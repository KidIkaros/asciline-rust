//! Playback queue: single video, folder scan, or JSON playlist.

use std::fs;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::protocol::{calc_auto_dimensions, MAX_GRID_COLS, MAX_GRID_ROWS};

const SUPPORTED_EXT: &[&str] = &[".mp4", ".mkv", ".avi", ".mov", ".webm"];

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub video: String,
    pub mode: u8,
    pub vol: u8,
    pub pixel: bool,
    /// Explicit rows (0 = auto).
    pub rows: u32,
    /// Per-entry cols override.
    pub cols_override: Option<u32>,
    pub is_webcam: bool,
    pub mirror: bool,
    pub fallback_fps: f64,
}

impl QueueEntry {
    pub fn from_file(path: String, mode: u8, vol: u8, pixel: bool, rows: u32, cols: Option<u32>) -> QueueEntry {
        QueueEntry {
            video: path,
            mode,
            vol,
            pixel,
            rows,
            cols_override: cols,
            is_webcam: false,
            mirror: false,
            fallback_fps: 0.0,
        }
    }

    pub fn default_cols(&self) -> u32 {
        if self.pixel {
            450
        } else {
            200
        }
    }

    /// Resolve (cols, rows) for this entry against probed source dimensions.
    /// Both values are clamped: a playlist entry or CLI flag must not be able
    /// to ask ffmpeg for a gigantic grid.
    pub fn resolve_cols_rows(&self, vid_w: u32, vid_h: u32) -> (u32, u32) {
        let cols = self
            .cols_override
            .unwrap_or_else(|| self.default_cols())
            .clamp(1, MAX_GRID_COLS);
        if self.rows > 0 {
            (cols, self.rows.clamp(1, MAX_GRID_ROWS))
        } else {
            calc_auto_dimensions(cols, vid_w, vid_h, self.pixel)
        }
    }
}

#[derive(Deserialize, Default)]
struct PlaylistItem {
    video: String,
    #[serde(default)]
    mode: Option<u8>,
    #[serde(default)]
    vol: Option<u8>,
    #[serde(default)]
    pixel: Option<bool>,
    #[serde(default)]
    rows: Option<u32>,
    #[serde(default)]
    cols: Option<u32>,
}

/// Load a JSON playlist. Each entry can override mode/vol/pixel/cols.
pub fn load_playlist(path: &str, def: &QueueEntry) -> Result<Vec<QueueEntry>> {
    let data = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read playlist {path:?}: {e}"))?;
    let items: Vec<PlaylistItem> = serde_json::from_str(&data)
        .map_err(|e| anyhow::anyhow!("invalid playlist JSON {path:?}: {e}"))?;
    Ok(items
        .into_iter()
        .map(|it| QueueEntry {
            video: it.video,
            mode: it.mode.unwrap_or(def.mode),
            vol: it.vol.unwrap_or(def.vol),
            pixel: it.pixel.unwrap_or(def.pixel),
            rows: it.rows.unwrap_or(def.rows),
            cols_override: it.cols.or(def.cols_override),
            is_webcam: false,
            mirror: false,
            fallback_fps: 0.0,
        })
        .collect())
}

/// Scan a folder for video files, in filesystem (directory) order.
pub fn load_folder(folder: &str, def: &QueueEntry) -> Result<Vec<QueueEntry>> {
    let mut entries = Vec::new();
    let rd = fs::read_dir(folder).map_err(|e| anyhow::anyhow!("cannot read folder {folder:?}: {e}"))?;
    for item in rd.flatten() {
        let path = item.path();
        if path.is_file() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if SUPPORTED_EXT.iter().any(|e| name.ends_with(e)) {
                entries.push(QueueEntry {
                    video: path.to_string_lossy().into_owned(),
                    ..def.clone()
                });
            }
        }
    }
    if entries.is_empty() {
        bail!("no supported video files found in {folder:?}");
    }
    Ok(entries)
}

/// Resolve a video argument: check as-is, then against `./videos/`.
pub fn resolve_video_path(video: &str) -> String {
    let candidates = [
        video.to_string(),
        Path::new("videos").join(Path::new(video).file_name().unwrap_or_default()).to_string_lossy().into_owned(),
    ];
    candidates
        .iter()
        .find(|p| Path::new(p).exists())
        .cloned()
        .unwrap_or_else(|| video.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_dimensions_are_clamped() {
        // playlist/CLI overrides must be clamped before reaching ffmpeg
        let e = QueueEntry {
            video: "x.mp4".into(),
            mode: 6,
            vol: 1,
            pixel: true,
            rows: 100_000,
            cols_override: Some(100_000),
            is_webcam: false,
            mirror: false,
            fallback_fps: 0.0,
        };
        let (c, r) = e.resolve_cols_rows(1920, 1080);
        assert_eq!(c, MAX_GRID_COLS);
        assert_eq!(r, MAX_GRID_ROWS);

        // explicit rows are clamped too; auto rows keep the aspect logic and
        // may rescale cols down to fit the pixel row cap — never blow up
        let e2 = QueueEntry {
            rows: 0,
            cols_override: Some(100_000),
            ..e
        };
        let (c2, r2) = e2.resolve_cols_rows(1920, 1080);
        assert!(c2 <= MAX_GRID_COLS && c2 > 0, "auto cols must stay bounded, got {c2}");
        assert!(r2 <= MAX_GRID_ROWS && r2 > 0, "auto rows must stay bounded, got {r2}");
    }
}
