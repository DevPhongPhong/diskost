//! UDP datagrams for one compressed and encrypted sample.
//!
//! Wire version 1, little-endian. The C++ server parses the same layout and
//! ignores the payload. Every datagram is at most [`MAX_DATAGRAM`] bytes.
//!
//! ```text
//!  0 magic "DKST"
//!  4 version u8 = 1
//!  5 source u8
//!  6 flags u8
//!  7 seq u32
//! 11 timestamp_ms u64
//! 19 frame_id u32
//! 23 chunk_index u16
//! 25 chunk_count u16
//! 27 room_len u8 | room | part_len u8 | participant | payload
//! ```

use crate::sample::{MediaSample, SourceKind};

pub const MAX_DATAGRAM: usize = 1200;
pub const VERSION: u8 = 1;
pub const FLAG_COMPRESSED: u8 = 1;
pub const FLAG_ENCRYPTED: u8 = 2;
const PREFIX: usize = 28;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Datagram {
    pub source: SourceKind,
    pub flags: u8,
    pub seq: u32,
    pub timestamp_ms: u64,
    pub frame_id: u32,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub room_id: String,
    pub participant_id: String,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub struct PacketError(pub String);

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn encode(packet: &Datagram) -> Result<Vec<u8>, PacketError> {
    if packet.room_id.len() > 255 || packet.participant_id.len() > 255 {
        return Err(PacketError("room or participant id is too long".into()));
    }
    let mut out = Vec::with_capacity(PREFIX + packet.room_id.len() + packet.participant_id.len() + packet.payload.len());
    out.extend_from_slice(b"DKST");
    out.push(VERSION);
    out.push(packet.source.as_u8());
    out.push(packet.flags);
    out.extend_from_slice(&packet.seq.to_le_bytes());
    out.extend_from_slice(&packet.timestamp_ms.to_le_bytes());
    out.extend_from_slice(&packet.frame_id.to_le_bytes());
    out.extend_from_slice(&packet.chunk_index.to_le_bytes());
    out.extend_from_slice(&packet.chunk_count.to_le_bytes());
    out.push(packet.room_id.len() as u8);
    out.extend_from_slice(packet.room_id.as_bytes());
    out.push(packet.participant_id.len() as u8);
    out.extend_from_slice(packet.participant_id.as_bytes());
    out.extend_from_slice(&packet.payload);
    if out.len() > MAX_DATAGRAM {
        return Err(PacketError(format!("datagram is {} bytes", out.len())));
    }
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<Datagram, PacketError> {
    if bytes.len() < PREFIX || &bytes[0..4] != b"DKST" {
        return Err(PacketError("bad magic".into()));
    }
    if bytes[4] != VERSION {
        return Err(PacketError("unsupported version".into()));
    }
    let source = SourceKind::from_u8(bytes[5]).ok_or_else(|| PacketError("unknown source".into()))?;
    let seq = u32::from_le_bytes(bytes[7..11].try_into().unwrap());
    let timestamp_ms = u64::from_le_bytes(bytes[11..19].try_into().unwrap());
    let frame_id = u32::from_le_bytes(bytes[19..23].try_into().unwrap());
    let chunk_index = u16::from_le_bytes(bytes[23..25].try_into().unwrap());
    let chunk_count = u16::from_le_bytes(bytes[25..27].try_into().unwrap());
    let room_len = bytes[27] as usize;
    let room_at = PREFIX;
    if bytes.len() < room_at + room_len + 1 {
        return Err(PacketError("truncated room id".into()));
    }
    let room_id = std::str::from_utf8(&bytes[room_at..room_at + room_len])
        .map_err(|_| PacketError("room id is not utf-8".into()))?
        .to_owned();
    let part_len_at = room_at + room_len;
    let part_len = bytes[part_len_at] as usize;
    let part_at = part_len_at + 1;
    if bytes.len() < part_at + part_len {
        return Err(PacketError("truncated participant id".into()));
    }
    let participant_id = std::str::from_utf8(&bytes[part_at..part_at + part_len])
        .map_err(|_| PacketError("participant id is not utf-8".into()))?
        .to_owned();
    let payload = bytes[part_at + part_len..].to_vec();
    Ok(Datagram {
        source,
        flags: bytes[6],
        seq,
        timestamp_ms,
        frame_id,
        chunk_index,
        chunk_count,
        room_id,
        participant_id,
        payload,
    })
}

pub fn split_sample(sample: &MediaSample, room_id: &str, participant_id: &str) -> Result<Vec<Datagram>, PacketError> {
    let probe = Datagram {
        source: sample.source,
        flags: FLAG_COMPRESSED | FLAG_ENCRYPTED,
        seq: sample.seq as u32,
        timestamp_ms: sample.timestamp_ms,
        frame_id: sample.seq as u32,
        chunk_index: 0,
        chunk_count: 1,
        room_id: room_id.to_owned(),
        participant_id: participant_id.to_owned(),
        payload: Vec::new(),
    };
    let header = encode(&probe)?.len();
    let budget = MAX_DATAGRAM - header;
    if budget == 0 {
        return Err(PacketError("header leaves no room for payload".into()));
    }
    let chunks = sample.bytes.chunks(budget).map(|chunk| chunk.to_vec()).collect::<Vec<_>>();
    let chunk_count = u16::try_from(chunks.len()).map_err(|_| PacketError("too many chunks".into()))?;
    if chunk_count == 0 {
        return Ok(Vec::new());
    }
    let mut packets = Vec::with_capacity(chunks.len());
    for (index, payload) in chunks.into_iter().enumerate() {
        packets.push(Datagram {
            chunk_index: index as u16,
            chunk_count,
            payload,
            ..probe.clone()
        });
    }
    Ok(packets)
}

#[derive(Default)]
pub struct FrameAssembler {
    parts: std::collections::HashMap<(SourceKind, u32), Vec<Option<Vec<u8>>>>,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the joined payload once every chunk of a frame has arrived.
    pub fn push(&mut self, packet: Datagram) -> Option<Vec<u8>> {
        if packet.chunk_count == 0 || packet.chunk_index >= packet.chunk_count {
            return None;
        }
        let key = (packet.source, packet.frame_id);
        let slots = self.parts.entry(key).or_insert_with(|| vec![None; packet.chunk_count as usize]);
        if slots.len() != packet.chunk_count as usize {
            *slots = vec![None; packet.chunk_count as usize];
        }
        slots[packet.chunk_index as usize] = Some(packet.payload);
        if slots.iter().any(|part| part.is_none()) {
            return None;
        }
        let ready = self.parts.remove(&key)?;
        let mut payload = Vec::new();
        for part in ready.into_iter().flatten() {
            payload.extend(part);
        }
        Some(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Stage;

    #[test]
    fn golden_header_matches_the_server() {
        let packet = Datagram {
            source: SourceKind::AudioIn,
            flags: FLAG_COMPRESSED | FLAG_ENCRYPTED,
            seq: 7,
            timestamp_ms: 1000,
            frame_id: 7,
            chunk_index: 0,
            chunk_count: 1,
            room_id: "AB".into(),
            participant_id: "p1".into(),
            payload: vec![1, 2, 3],
        };
        let bytes = encode(&packet).unwrap();
        assert_eq!(
            bytes,
            [
                b'D', b'K', b'S', b'T', 1, 1, 3, 7, 0, 0, 0, 0xe8, 3, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0,
                0, 0, 1, 0, 2, b'A', b'B', 2, b'p', b'1', 1, 2, 3,
            ]
        );
        assert_eq!(decode(&bytes).unwrap(), packet);
    }

    #[test]
    fn chunks_reassemble_and_stay_under_the_limit() {
        let mut sample = MediaSample::video(SourceKind::Display, 9, 8, 8, vec![7u8; 5_000]);
        sample.stage = Stage::Encrypted;
        let packets = split_sample(&sample, "ROOM", "person").unwrap();
        assert!(packets.len() > 1);
        let mut assembler = FrameAssembler::new();
        let mut joined = None;
        for packet in packets {
            let bytes = encode(&packet).unwrap();
            assert!(bytes.len() <= MAX_DATAGRAM);
            if let Some(payload) = assembler.push(decode(&bytes).unwrap()) {
                joined = Some(payload);
            }
        }
        assert_eq!(joined.unwrap(), sample.bytes);
    }
}
