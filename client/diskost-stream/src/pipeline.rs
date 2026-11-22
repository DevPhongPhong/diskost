//! Wires sources to the compressor, the encryptor, and the UDP sender.
//!
//! The lobby device test reads [`Stage::Raw`] and does not start this pipeline.
//! A room recording does. Both share [`crate::DataStream`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::codec::{compress_sample, decompress_sample};
use crate::devices::DeviceSession;
use crate::encrypt::{decrypt_sample, encrypt_sample};
use crate::sample::{MediaSample, SourceKind, Stage};
use crate::stream::DataStream;
use crate::udp::UdpUplink;

#[derive(Clone, Debug)]
pub struct UplinkConfig {
    pub remote: std::net::SocketAddr,
    pub room_id: String,
    pub participant_id: String,
    pub key: [u8; 32],
    pub mic_id: Option<String>,
    pub display_id: Option<String>,
    pub webcam_id: Option<String>,
}

pub struct RecordingPipeline {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    stream: DataStream,
    session: DeviceSession,
}

impl RecordingPipeline {
    /// Processors and the UDP sender, with no devices opened yet.
    pub fn start_workers(config: UplinkConfig) -> Result<Self, String> {
        let stream = DataStream::new();
        let stop = Arc::new(AtomicBool::new(false));
        let raw_rx = stream.subscribe(None, Stage::Raw);
        let compressed_rx = stream.subscribe(None, Stage::Compressed);
        let encrypted_rx = stream.subscribe(None, Stage::Encrypted);

        let compress_bus = stream.clone();
        let compress_stop = Arc::clone(&stop);
        let compress = thread::spawn(move || {
            worker(compress_stop, raw_rx, |sample| {
                if sample.stage != Stage::Raw {
                    return;
                }
                match compress_sample(&sample) {
                    Ok(compressed) => compress_bus.publish(compressed),
                    Err(err) => log::warn!("compress: {err}"),
                }
            });
        });

        let encrypt_bus = stream.clone();
        let encrypt_stop = Arc::clone(&stop);
        let key = config.key;
        let encrypt = thread::spawn(move || {
            worker(encrypt_stop, compressed_rx, move |sample| {
                if sample.stage != Stage::Compressed {
                    return;
                }
                match encrypt_sample(&key, &sample) {
                    Ok(sealed) => encrypt_bus.publish(sealed),
                    Err(err) => log::warn!("encrypt: {err}"),
                }
            });
        });

        let uplink = UdpUplink::connect(config.remote, config.room_id.clone(), config.participant_id.clone())?;
        let send_stop = Arc::clone(&stop);
        let send = thread::spawn(move || {
            worker(send_stop, encrypted_rx, move |sample| {
                if sample.stage != Stage::Encrypted {
                    return;
                }
                if let Err(err) = uplink.send_sample(&sample) {
                    log::warn!("udp: {err}");
                }
            });
        });

        Ok(Self {
            stop,
            threads: vec![compress, encrypt, send],
            session: DeviceSession::new(stream.clone()),
            stream,
        })
    }

    pub fn start(config: UplinkConfig) -> Result<Self, String> {
        let mut pipeline = Self::start_workers(config.clone())?;
        if let Some(id) = config.mic_id.as_deref() {
            pipeline.session.open_mic(id).map_err(|err| err.to_string())?;
        }
        if let Some(id) = config.webcam_id.as_deref() {
            pipeline.session.open_webcam(id).map_err(|err| err.to_string())?;
        }
        if let Some(id) = config.display_id.as_deref() {
            pipeline.session.open_display(id).map_err(|err| err.to_string())?;
        }
        Ok(pipeline)
    }

    pub fn stream(&self) -> DataStream {
        self.stream.clone()
    }

    /// Live raw samples for one source. This is how a caller reads recorded data.
    pub fn recorded(&self, source: SourceKind) -> Receiver<MediaSample> {
        self.stream.subscribe(Some(source), Stage::Raw)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.session.stop();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for RecordingPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker(stop: Arc<AtomicBool>, rx: Receiver<MediaSample>, mut step: impl FnMut(MediaSample)) {
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(sample) => step(sample),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Undo encryption and compression. Used by tests and by anything that stored
/// the encrypted payload and still has the key.
pub fn decode_recorded(key: &[u8; 32], encrypted: &MediaSample) -> Result<MediaSample, String> {
    let compressed = decrypt_sample(key, encrypted)?;
    decompress_sample(&compressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, FrameAssembler};
    use std::net::UdpSocket;
    use std::time::Duration;

    #[test]
    fn raw_sample_arrives_compressed_and_encrypted() {
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let key = [11u8; 32];
        let pipeline = RecordingPipeline::start_workers(UplinkConfig {
            remote: listener.local_addr().unwrap(),
            room_id: "AB".into(),
            participant_id: "p1".into(),
            key,
            mic_id: None,
            display_id: None,
            webcam_id: None,
        })
        .unwrap();
        let pixels: Vec<u8> = (0..16).collect();
        pipeline
            .stream()
            .publish(MediaSample::video(SourceKind::Display, 4, 2, 2, pixels.clone()));

        let mut buf = [0u8; 1500];
        let (size, _) = listener.recv_from(&mut buf).expect("udp payload");
        let packet = decode(&buf[..size]).unwrap();
        assert_eq!(packet.room_id, "AB");
        let payload = FrameAssembler::new().push(packet).unwrap();
        let mut sealed = MediaSample::video(SourceKind::Display, 4, 2, 2, payload);
        sealed.stage = Stage::Encrypted;
        let back = decode_recorded(&key, &sealed).unwrap();
        assert_eq!(back.bytes, pixels);
        assert_eq!(back.width, 2);
    }
}
