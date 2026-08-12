//! Audio streaming for the web player.
//!
//! The `/audio` endpoint transcodes the current video's audio track to MP3 via
//! ffmpeg (server-side volume applied with an ffmpeg `volume=` filter) and
//! streams it. `vol` 0 mutes without ever spawning ffmpeg, matching the Python
//! server's CPU/bandwidth saving.

/// Map the 0-5 volume knob to an ffmpeg volume multiplier.
/// 1 → 1.0x, 5 → 2.0x, per the original.
pub fn ffmpeg_volume(vol_level: u8) -> f64 {
    1.0 + (vol_level.saturating_sub(1) as f64) * 0.25
}
