//! Capture, compress, encrypt, and ship a room recording.
//!
//! Live calls still use the WebRTC mesh. This crate is the side path: devices
//! publish raw samples on [`DataStream`], processors republish compressed and
//! encrypted samples, and [`UdpUplink`] sends those to the server.

mod codec;
mod devices;
mod encrypt;
mod packet;
mod pipeline;
#[cfg_attr(not(feature = "capture"), allow(dead_code))]
mod resample;
mod sample;
mod stream;
mod udp;

pub use codec::{compress_sample, decompress_sample, Codec, MediaCodec};
pub use devices::{DeviceError, DeviceInfo, DeviceKind, DeviceManager, DeviceSession};
pub use encrypt::{decrypt_sample, encrypt_sample, open, random_key, seal};
pub use packet::{decode, encode, split_sample, Datagram, FrameAssembler, FLAG_COMPRESSED, FLAG_ENCRYPTED, MAX_DATAGRAM};
pub use pipeline::{decode_recorded, RecordingPipeline, UplinkConfig};
pub use sample::{now_ms, MediaSample, SourceKind, Stage};
pub use stream::DataStream;
pub use udp::{udp_target, UdpUplink};
