//! ASCILINE (Rust) — real-time ASCII video rendering engine.
//!
//! A Rust port of the Python `ASCILINE` project (https://github.com/YusufB5/ASCILINE):
//! decode a video with ffmpeg, map pixels to ASCII characters / colored blocks,
//! compress each frame with an adaptive codec (RAW/ZLIB/DELTA/RLE_FULL), and stream
//! it over WebSocket to the (unchanged) original browser client, or render it
//! directly in a true-color terminal.
//!
//! The wire protocol is byte-compatible with the original implementation:
//!   - INIT text message: `INIT:{fps}:{mode}:{cols}:{rows}:{pixel}:{queue_idx}:{duration}:{seek}:{webcam}`
//!   - Binary frames: `[4B frame_index BE][1B tag][payload]`
//!     tag 0 RAW / 1 ZLIB / 2 DELTA / 3 RLE_FULL
//!   - `.ascf` static files: 18-byte `ASC2` header + length-prefixed frame records.

pub mod audio;
pub mod codec;
pub mod filters;
pub mod mapper;
pub mod profile;
pub mod protocol;
pub mod quality;
pub mod queue;
pub mod server;
pub mod video;

pub use codec::{CodecDecoder, CodecEncoder};
pub use mapper::{Mapper, Palette};
pub use profile::{ProfileDecoder, ProfileEncoder};
pub use video::{probe_video, FrameReader, VideoInfo};

/// The default ASCII ramp (93 levels), identical to the original project.
pub const DEFAULT_PALETTE: &str =
    " `.-':_,^=;><+!rc*/z?sLTv)J7(|Fi{C}fI31tlu[neoZ5Yxjya]2ESwqkP6h9d4VpOGbUAKXHm8RD#$Bg0MNWQ%&@";

/// Flat / anime palette (server filter).
pub const FLAT_PALETTE: &str = " .:-=+*#%@";
/// Block palette (server filter).
pub const BLOCK_PALETTE: &str = " .+o#@";
