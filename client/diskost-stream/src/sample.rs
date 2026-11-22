//! One captured or processed chunk on the data stream.

use std::time::{SystemTime, UNIX_EPOCH};

/// Where a sample came from. Numeric values are the on-wire source ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    AudioIn = 1,
    Webcam = 2,
    Display = 3,
}

impl SourceKind {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::AudioIn),
            2 => Some(Self::Webcam),
            3 => Some(Self::Display),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// How far along the processors a sample is.
///
/// Sources publish [`Stage::Raw`]. The compressor republishes
/// [`Stage::Compressed`]. The encryptor republishes [`Stage::Encrypted`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    Raw,
    Compressed,
    Encrypted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaSample {
    pub source: SourceKind,
    pub stage: Stage,
    pub seq: u64,
    pub timestamp_ms: u64,
    /// Sample rate for audio. Zero for video frames.
    pub sample_rate: u32,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

impl MediaSample {
    pub fn audio(seq: u64, sample_rate: u32, pcm_s16le: Vec<u8>) -> Self {
        Self {
            source: SourceKind::AudioIn,
            stage: Stage::Raw,
            seq,
            timestamp_ms: now_ms(),
            sample_rate,
            width: 0,
            height: 0,
            bytes: pcm_s16le,
        }
    }

    pub fn video(source: SourceKind, seq: u64, width: u32, height: u32, pixels: Vec<u8>) -> Self {
        Self {
            source,
            stage: Stage::Raw,
            seq,
            timestamp_ms: now_ms(),
            sample_rate: 0,
            width,
            height,
            bytes: pixels,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
