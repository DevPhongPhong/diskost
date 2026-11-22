//! Audio and picture codecs.
//!
//! Audio is IMA ADPCM (lossy, about 4:1). Pictures are LZ4 over the raw pixels
//! and round-trip to the same bytes. Both go through [`Codec`] so a later Opus
//! encoder can replace ADPCM without touching the bus or the UDP header.

use crate::sample::{MediaSample, SourceKind, Stage};

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

pub trait Codec {
    fn compress(&self, sample: &MediaSample) -> Result<MediaSample, String>;
    fn decompress(&self, sample: &MediaSample) -> Result<MediaSample, String>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MediaCodec;

impl Codec for MediaCodec {
    fn compress(&self, sample: &MediaSample) -> Result<MediaSample, String> {
        compress_sample(sample)
    }

    fn decompress(&self, sample: &MediaSample) -> Result<MediaSample, String> {
        decompress_sample(sample)
    }
}

pub fn compress_sample(sample: &MediaSample) -> Result<MediaSample, String> {
    if sample.stage != Stage::Raw {
        return Err("compressor expects a raw sample".into());
    }
    let bytes = match sample.source {
        SourceKind::AudioIn => compress_audio(sample.sample_rate, &sample.bytes)?,
        SourceKind::Webcam | SourceKind::Display => {
            compress_picture(sample.width, sample.height, &sample.bytes)?
        }
    };
    let mut out = sample.clone();
    out.stage = Stage::Compressed;
    out.bytes = bytes;
    Ok(out)
}

pub fn decompress_sample(sample: &MediaSample) -> Result<MediaSample, String> {
    if sample.stage != Stage::Compressed {
        return Err("decompressor expects a compressed sample".into());
    }
    let mut out = sample.clone();
    match sample.source {
        SourceKind::AudioIn => {
            let (rate, pcm) = decompress_audio(&sample.bytes)?;
            out.sample_rate = rate;
            out.bytes = pcm;
        }
        SourceKind::Webcam | SourceKind::Display => {
            let (width, height, pixels) = decompress_picture(&sample.bytes)?;
            out.width = width;
            out.height = height;
            out.bytes = pixels;
        }
    }
    out.stage = Stage::Raw;
    Ok(out)
}

pub fn compress_audio(sample_rate: u32, pcm_s16le: &[u8]) -> Result<Vec<u8>, String> {
    if pcm_s16le.len() % 2 != 0 {
        return Err("pcm length is not a whole number of samples".into());
    }
    let samples = pcm_s16le.len() / 2;
    let mut pred = 0i32;
    let mut index = 0i32;
    let mut nibbles = Vec::with_capacity(samples);
    for chunk in pcm_s16le.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        nibbles.push(encode_nibble(&mut pred, &mut index, sample as i32));
    }
    let mut out = Vec::with_capacity(12 + nibbles.len().div_ceil(2));
    out.extend_from_slice(b"ADPC");
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(samples as u32).to_le_bytes());
    let mut i = 0;
    while i + 1 < nibbles.len() {
        out.push(nibbles[i] | (nibbles[i + 1] << 4));
        i += 2;
    }
    if i < nibbles.len() {
        out.push(nibbles[i]);
    }
    Ok(out)
}

pub fn decompress_audio(bytes: &[u8]) -> Result<(u32, Vec<u8>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"ADPC" {
        return Err("audio frame is not ADPCM".into());
    }
    let rate = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut nibbles = Vec::with_capacity(count);
    for byte in &bytes[12..] {
        nibbles.push(byte & 0x0f);
        nibbles.push(byte >> 4);
    }
    if nibbles.len() < count {
        return Err("ADPCM frame is shorter than its sample count".into());
    }
    let mut pred = 0i32;
    let mut index = 0i32;
    let mut pcm = Vec::with_capacity(count * 2);
    for nibble in nibbles.into_iter().take(count) {
        let sample = decode_nibble(&mut pred, &mut index, nibble);
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    Ok((rate, pcm))
}

pub fn compress_picture(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, String> {
    let packed = lz4_flex::compress_prepend_size(pixels);
    let mut out = Vec::with_capacity(12 + packed.len());
    out.extend_from_slice(b"LZ4F");
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&packed);
    Ok(out)
}

pub fn decompress_picture(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"LZ4F" {
        return Err("picture frame is not LZ4".into());
    }
    let width = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let pixels = lz4_flex::decompress_size_prepended(&bytes[12..]).map_err(|err| err.to_string())?;
    Ok((width, height, pixels))
}

fn encode_nibble(pred: &mut i32, index: &mut i32, sample: i32) -> u8 {
    let step = STEP_TABLE[*index as usize];
    let mut diff = sample - *pred;
    let mut nibble = 0u8;
    if diff < 0 {
        nibble = 8;
        diff = -diff;
    }
    if diff >= step {
        nibble |= 4;
        diff -= step;
    }
    if diff >= step >> 1 {
        nibble |= 2;
        diff -= step >> 1;
    }
    if diff >= step >> 2 {
        nibble |= 1;
    }
    apply_nibble(pred, index, nibble);
    nibble
}

fn decode_nibble(pred: &mut i32, index: &mut i32, nibble: u8) -> i16 {
    apply_nibble(pred, index, nibble);
    *pred as i16
}

fn apply_nibble(pred: &mut i32, index: &mut i32, nibble: u8) {
    let step = STEP_TABLE[*index as usize];
    let mut delta = step >> 3;
    if nibble & 4 != 0 {
        delta += step;
    }
    if nibble & 2 != 0 {
        delta += step >> 1;
    }
    if nibble & 1 != 0 {
        delta += step >> 2;
    }
    if nibble & 8 != 0 {
        *pred -= delta;
    } else {
        *pred += delta;
    }
    *pred = (*pred).clamp(-32768, 32767);
    *index = (*index + INDEX_TABLE[nibble as usize]).clamp(0, 88);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_roundtrip_is_exact() {
        let pixels: Vec<u8> = (0..16).collect();
        let frame = compress_picture(2, 2, &pixels).unwrap();
        let (width, height, back) = decompress_picture(&frame).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!(back, pixels);
    }

    #[test]
    fn adpcm_stays_close_to_the_source() {
        let pcm: Vec<i16> = (0..400)
            .map(|i| ((i as f32 / 32.0).sin() * 8000.0) as i16)
            .collect();
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in &pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let packed = compress_audio(16_000, &bytes).unwrap();
        let (rate, decoded) = decompress_audio(&packed).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(decoded.len(), bytes.len());
        let mut error = 0i32;
        for (index, chunk) in decoded.chunks_exact(2).enumerate() {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            error += (pcm[index] as i32 - sample as i32).abs();
        }
        let mean = error / pcm.len() as i32;
        assert!(mean < 800, "mean absolute error {mean}");
    }

    #[test]
    fn codec_trait_marks_stage() {
        let codec = MediaCodec;
        let raw = MediaSample::video(SourceKind::Display, 4, 1, 1, vec![1, 2, 3, 4]);
        let compressed = codec.compress(&raw).unwrap();
        assert_eq!(compressed.stage, Stage::Compressed);
        let back = codec.decompress(&compressed).unwrap();
        assert_eq!(back.bytes, raw.bytes);
        assert_eq!(back.stage, Stage::Raw);
    }
}
