//! Open and close microphones, speakers, webcams, and displays.
//!
//! Hardware crates are behind the `capture` feature. Without it, listing
//! returns an empty set and open calls report [`DeviceError::Unavailable`].
//! Every captured sample is published on the data stream as raw. The speaker
//! does not capture; a bridge reads microphone samples from that bus and
//! fills the speaker ring.

use std::fmt;

use crate::stream::DataStream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Mic,
    Speaker,
    Webcam,
    Display,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub label: String,
    pub kind: DeviceKind,
}

#[derive(Debug)]
pub enum DeviceError {
    Missing(String),
    Open(String),
    Unavailable(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(id) => write!(f, "device not found: {id}"),
            Self::Open(message) => write!(f, "{message}"),
            Self::Unavailable(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DeviceError {}

pub struct DeviceManager;

#[cfg(not(feature = "capture"))]
mod platform {
    use super::*;

    pub struct DeviceSession {
        stream: DataStream,
    }

    impl DeviceManager {
        pub fn refresh() -> Vec<DeviceInfo> {
            Vec::new()
        }

        pub fn default_id(_kind: DeviceKind) -> Option<String> {
            None
        }
    }

    impl DeviceSession {
        pub fn new(stream: DataStream) -> Self {
            Self { stream }
        }

        pub fn stream(&self) -> DataStream {
            self.stream.clone()
        }

        pub fn mic_rate(&self) -> u32 {
            0
        }

        pub fn speaker_rate(&self) -> u32 {
            0
        }

        pub fn underruns(&self) -> u64 {
            0
        }

        pub fn open_mic(&mut self, _id: &str) -> Result<(), DeviceError> {
            Err(disabled())
        }

        pub fn open_speaker(&mut self, _id: &str) -> Result<(), DeviceError> {
            Err(disabled())
        }

        pub fn open_webcam(&mut self, _id: &str) -> Result<(), DeviceError> {
            Err(disabled())
        }

        pub fn open_display(&mut self, _id: &str) -> Result<(), DeviceError> {
            Err(disabled())
        }

        pub fn start_mic_monitor(&mut self) -> Result<(), DeviceError> {
            Err(disabled())
        }

        pub fn stop_mic_monitor(&mut self) {}
        pub fn stop_mic(&mut self) {}
        pub fn stop_speaker(&mut self) {}
        pub fn stop_webcam(&mut self) {}
        pub fn stop_display(&mut self) {}
        pub fn stop(&mut self) {}
    }

    fn disabled() -> DeviceError {
        DeviceError::Unavailable("built without the capture feature".into())
    }
}

#[cfg(feature = "capture")]
mod platform {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::RecvTimeoutError;
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType};
    use nokhwa::{query, Camera};

    use super::*;
    use crate::resample::Resampler;
    use crate::sample::{MediaSample, SourceKind, Stage};

    const FRAME_INTERVAL: Duration = Duration::from_millis(100);

    struct AudioRing {
        samples: Mutex<VecDeque<f32>>,
        capacity: usize,
        underruns: AtomicU64,
    }

    impl AudioRing {
        fn new(capacity: usize) -> Self {
            Self {
                samples: Mutex::new(VecDeque::with_capacity(capacity)),
                capacity: capacity.max(1),
                underruns: AtomicU64::new(0),
            }
        }

        fn push(&self, frames: &[f32]) {
            let mut queue = self.samples.lock().unwrap_or_else(|err| err.into_inner());
            for sample in frames {
                if queue.len() >= self.capacity {
                    queue.pop_front();
                }
                queue.push_back(*sample);
            }
        }

        fn pull_f32(&self, dst: &mut [f32], channels: usize) -> bool {
            let channels = channels.max(1);
            let frames = dst.len() / channels;
            let mut missing = false;
            let mut queue = self.samples.lock().unwrap_or_else(|err| err.into_inner());
            for frame in 0..frames {
                let sample = match queue.pop_front() {
                    Some(sample) => sample,
                    None => {
                        missing = true;
                        0.0
                    }
                };
                for channel in 0..channels {
                    dst[frame * channels + channel] = sample;
                }
            }
            missing
        }
    }

    struct Speaker {
        stream: cpal::Stream,
        ring: Arc<AudioRing>,
        rate: u32,
        channels: u16,
    }

    pub struct DeviceSession {
        stream: DataStream,
        mic: Option<cpal::Stream>,
        mic_rate: u32,
        mic_seq: Arc<AtomicU64>,
        speaker: Option<Speaker>,
        monitor_stop: Option<Arc<AtomicBool>>,
        monitor: Option<JoinHandle<()>>,
        webcam_stop: Option<Arc<AtomicBool>>,
        webcam: Option<JoinHandle<()>>,
        display_stop: Option<Arc<AtomicBool>>,
        display: Option<JoinHandle<()>>,
        display_seq: Arc<AtomicU64>,
        webcam_seq: Arc<AtomicU64>,
    }

    impl DeviceManager {
        pub fn refresh() -> Vec<DeviceInfo> {
            let mut devices = Vec::new();
            devices.extend(audio_list(true).into_iter().map(|(info, _)| info));
            devices.extend(audio_list(false).into_iter().map(|(info, _)| info));
            devices.extend(webcam_list());
            devices.extend(display_list());
            devices
        }

        pub fn default_id(kind: DeviceKind) -> Option<String> {
            let devices = Self::refresh();
            match kind {
                DeviceKind::Mic => named_default(true, &devices),
                DeviceKind::Speaker => named_default(false, &devices),
                DeviceKind::Webcam => first_id(&devices, kind),
                DeviceKind::Display => primary_display().or_else(|| first_id(&devices, kind)),
            }
        }
    }

    impl DeviceSession {
        pub fn new(stream: DataStream) -> Self {
            Self {
                stream,
                mic: None,
                mic_rate: 0,
                mic_seq: Arc::new(AtomicU64::new(0)),
                speaker: None,
                monitor_stop: None,
                monitor: None,
                webcam_stop: None,
                webcam: None,
                display_stop: None,
                display: None,
                display_seq: Arc::new(AtomicU64::new(0)),
                webcam_seq: Arc::new(AtomicU64::new(0)),
            }
        }

        pub fn stream(&self) -> DataStream {
            self.stream.clone()
        }

        pub fn mic_rate(&self) -> u32 {
            self.mic_rate
        }

        pub fn speaker_rate(&self) -> u32 {
            self.speaker.as_ref().map(|speaker| speaker.rate).unwrap_or(0)
        }

        pub fn underruns(&self) -> u64 {
            self.speaker
                .as_ref()
                .map(|speaker| speaker.ring.underruns.load(Ordering::Relaxed))
                .unwrap_or(0)
        }

        pub fn open_mic(&mut self, id: &str) -> Result<(), DeviceError> {
            self.stop_mic();
            let device = take_audio(id, true)?;
            let supported = device
                .default_input_config()
                .map_err(|err| DeviceError::Open(err.to_string()))?;
            let channels = supported.channels();
            let rate = supported.sample_rate();
            let format = supported.sample_format();
            let config: StreamConfig = supported.into();
            let bus = self.stream.clone();
            let seq = Arc::clone(&self.mic_seq);
            let stream = device
                .build_input_stream_raw(
                    config,
                    format,
                    move |data, _| {
                        let pcm = to_mono_s16(data.bytes(), format, channels);
                        if pcm.is_empty() {
                            return;
                        }
                        bus.publish(MediaSample::audio(
                            seq.fetch_add(1, Ordering::Relaxed),
                            rate,
                            s16_bytes(&pcm),
                        ));
                    },
                    |err| log::warn!("mic: {err}"),
                    None,
                )
                .map_err(|err| DeviceError::Open(err.to_string()))?;
            stream.play().map_err(|err| DeviceError::Open(err.to_string()))?;
            self.mic = Some(stream);
            self.mic_rate = rate;
            Ok(())
        }

        pub fn open_speaker(&mut self, id: &str) -> Result<(), DeviceError> {
            self.stop_mic_monitor();
            self.stop_speaker();
            let device = take_audio(id, false)?;
            let supported = device
                .default_output_config()
                .map_err(|err| DeviceError::Open(err.to_string()))?;
            let channels = supported.channels();
            let rate = supported.sample_rate();
            let format = supported.sample_format();
            let config: StreamConfig = supported.into();
            let capacity = ((rate as usize) * 40 / 1000).max(1);
            let ring = Arc::new(AudioRing::new(capacity));
            let playback = Arc::clone(&ring);
            let channel_count = channels as usize;
            let stream = match format {
                SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
                    config,
                    move |data, _| {
                        if playback.pull_f32(data, channel_count) {
                            playback.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    |err| log::warn!("speaker: {err}"),
                    None,
                ),
                SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                    config,
                    move |data, _| {
                        pull_scaled(data, channel_count, &playback, |sample| {
                            (sample.clamp(-1.0, 1.0) * 32767.0) as i16
                        })
                    },
                    |err| log::warn!("speaker: {err}"),
                    None,
                ),
                SampleFormat::U16 => device.build_output_stream::<u16, _, _>(
                    config,
                    move |data, _| {
                        pull_scaled(data, channel_count, &playback, |sample| {
                            ((sample.clamp(-1.0, 1.0) + 1.0) * 0.5 * 65535.0) as u16
                        })
                    },
                    |err| log::warn!("speaker: {err}"),
                    None,
                ),
                other => {
                    return Err(DeviceError::Open(format!("unsupported speaker format {other}")));
                }
            }
            .map_err(|err| DeviceError::Open(err.to_string()))?;
            stream.play().map_err(|err| DeviceError::Open(err.to_string()))?;
            self.speaker = Some(Speaker {
                stream,
                ring,
                rate,
                channels,
            });
            Ok(())
        }

        pub fn open_webcam(&mut self, id: &str) -> Result<(), DeviceError> {
            self.stop_webcam();
            let index = parse_index(id, "webcam:")? as u32;
            let (opened_tx, opened_rx) = std::sync::mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let bus = self.stream.clone();
            let seq = Arc::clone(&self.webcam_seq);
            // Camera is not Send, so it is created on the capture thread.
            let handle = thread::spawn(move || {
                let requested =
                    RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
                let mut camera = match Camera::new(CameraIndex::Index(index), requested) {
                    Ok(camera) => camera,
                    Err(err) => {
                        let _ = opened_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                if let Err(err) = camera.open_stream() {
                    let _ = opened_tx.send(Err(err.to_string()));
                    return;
                }
                let _ = opened_tx.send(Ok(()));
                while !flag.load(Ordering::SeqCst) {
                    let frame = match camera.frame() {
                        Ok(frame) => frame,
                        Err(err) => {
                            log::warn!("webcam: {err}");
                            break;
                        }
                    };
                    let image = match frame.decode_image::<RgbFormat>() {
                        Ok(image) => image,
                        Err(err) => {
                            log::warn!("webcam decode: {err}");
                            continue;
                        }
                    };
                    bus.publish(MediaSample::video(
                        SourceKind::Webcam,
                        seq.fetch_add(1, Ordering::Relaxed),
                        image.width(),
                        image.height(),
                        image.into_raw(),
                    ));
                }
                let _ = camera.stop_stream();
            });
            match opened_rx.recv() {
                Ok(Ok(())) => {
                    self.webcam_stop = Some(stop);
                    self.webcam = Some(handle);
                    Ok(())
                }
                Ok(Err(err)) => {
                    let _ = handle.join();
                    Err(DeviceError::Open(err))
                }
                Err(_) => {
                    let _ = handle.join();
                    Err(DeviceError::Open("webcam thread stopped before it opened".into()))
                }
            }
        }

        pub fn open_display(&mut self, id: &str) -> Result<(), DeviceError> {
            self.stop_display();
            let index = parse_index(id, "display:")?;
            let monitors = xcap::Monitor::all().map_err(|err| DeviceError::Open(err.to_string()))?;
            let monitor = monitors
                .into_iter()
                .nth(index)
                .ok_or_else(|| DeviceError::Missing(id.to_owned()))?;
            let first = monitor
                .capture_image()
                .map_err(|err| DeviceError::Open(err.to_string()))?;
            self.publish_display(first.width(), first.height(), first.into_raw());
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let bus = self.stream.clone();
            let seq = Arc::clone(&self.display_seq);
            let handle = thread::spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    let started = std::time::Instant::now();
                    match monitor.capture_image() {
                        Ok(image) => {
                            bus.publish(MediaSample::video(
                                SourceKind::Display,
                                seq.fetch_add(1, Ordering::Relaxed),
                                image.width(),
                                image.height(),
                                image.into_raw(),
                            ));
                        }
                        Err(err) => {
                            log::warn!("display: {err}");
                            break;
                        }
                    }
                    let elapsed = started.elapsed();
                    if elapsed < FRAME_INTERVAL {
                        thread::sleep(FRAME_INTERVAL - elapsed);
                    }
                }
            });
            self.display_stop = Some(stop);
            self.display = Some(handle);
            Ok(())
        }

        pub fn start_mic_monitor(&mut self) -> Result<(), DeviceError> {
            self.stop_mic_monitor();
            let speaker = self.speaker.as_ref().ok_or_else(|| {
                DeviceError::Unavailable("open a speaker before monitoring the microphone".into())
            })?;
            let ring = Arc::clone(&speaker.ring);
            let speaker_rate = speaker.rate;
            let rx = self.stream.subscribe(Some(SourceKind::AudioIn), Stage::Raw);
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                let mut resampler = Resampler::new(speaker_rate, speaker_rate);
                while !flag.load(Ordering::SeqCst) {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(sample) => {
                            resampler.set_rates(sample.sample_rate, speaker_rate);
                            ring.push(&resampler.process(&s16le_to_f32(&sample.bytes)));
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            self.monitor_stop = Some(stop);
            self.monitor = Some(handle);
            Ok(())
        }

        pub fn stop_mic_monitor(&mut self) {
            if let Some(flag) = self.monitor_stop.take() {
                flag.store(true, Ordering::SeqCst);
            }
            if let Some(handle) = self.monitor.take() {
                let _ = handle.join();
            }
        }

        pub fn stop_mic(&mut self) {
            self.mic.take();
            self.mic_rate = 0;
        }

        pub fn stop_speaker(&mut self) {
            self.speaker.take();
        }

        pub fn stop_webcam(&mut self) {
            if let Some(flag) = self.webcam_stop.take() {
                flag.store(true, Ordering::SeqCst);
            }
            if let Some(handle) = self.webcam.take() {
                let _ = handle.join();
            }
        }

        pub fn stop_display(&mut self) {
            if let Some(flag) = self.display_stop.take() {
                flag.store(true, Ordering::SeqCst);
            }
            if let Some(handle) = self.display.take() {
                let _ = handle.join();
            }
        }

        pub fn stop(&mut self) {
            self.stop_mic_monitor();
            self.stop_mic();
            self.stop_speaker();
            self.stop_webcam();
            self.stop_display();
        }

        fn publish_display(&self, width: u32, height: u32, pixels: Vec<u8>) {
            self.stream.publish(MediaSample::video(
                SourceKind::Display,
                self.display_seq.fetch_add(1, Ordering::Relaxed),
                width,
                height,
                pixels,
            ));
        }
    }

    impl Drop for DeviceSession {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn first_id(devices: &[DeviceInfo], kind: DeviceKind) -> Option<String> {
        devices
            .iter()
            .find(|device| device.kind == kind)
            .map(|device| device.id.clone())
    }

    fn named_default(input: bool, devices: &[DeviceInfo]) -> Option<String> {
        let host = cpal::default_host();
        let device = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        }?;
        let name = device.to_string();
        let kind = if input { DeviceKind::Mic } else { DeviceKind::Speaker };
        devices
            .iter()
            .find(|info| info.kind == kind && info.label == name)
            .map(|info| info.id.clone())
    }

    fn primary_display() -> Option<String> {
        let monitors = xcap::Monitor::all().ok()?;
        if monitors.is_empty() {
            return None;
        }
        let index = monitors
            .iter()
            .position(|monitor| monitor.is_primary().unwrap_or(false))
            .unwrap_or(0);
        Some(format!("display:{index}"))
    }

    fn audio_list(input: bool) -> Vec<(DeviceInfo, cpal::Device)> {
        let host = cpal::default_host();
        let listed = if input {
            host.input_devices()
        } else {
            host.output_devices()
        };
        match listed {
            Ok(iter) => walk(iter, input),
            Err(err) => {
                log::warn!("audio devices: {err}");
                Vec::new()
            }
        }
    }

    fn walk(iter: impl Iterator<Item = cpal::Device>, input: bool) -> Vec<(DeviceInfo, cpal::Device)> {
        let prefix = if input { "mic" } else { "speaker" };
        let kind = if input { DeviceKind::Mic } else { DeviceKind::Speaker };
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut out = Vec::new();
        for device in iter {
            let name = device.to_string();
            let seen = counts.entry(name.clone()).or_insert(0);
            *seen += 1;
            let id_name = if *seen == 1 {
                name.clone()
            } else {
                format!("{name}#{}", *seen)
            };
            out.push((
                DeviceInfo {
                    id: format!("{prefix}:{id_name}"),
                    label: name,
                    kind,
                },
                device,
            ));
        }
        out
    }

    fn take_audio(id: &str, input: bool) -> Result<cpal::Device, DeviceError> {
        audio_list(input)
            .into_iter()
            .find(|(info, _)| info.id == id)
            .map(|(_, device)| device)
            .ok_or_else(|| DeviceError::Missing(id.to_owned()))
    }

    fn webcam_list() -> Vec<DeviceInfo> {
        match query(ApiBackend::Auto) {
            Ok(cameras) => cameras
                .into_iter()
                .filter_map(|info| {
                    let index = match info.index() {
                        CameraIndex::Index(value) => *value,
                        CameraIndex::String(_) => return None,
                    };
                    Some(DeviceInfo {
                        id: format!("webcam:{index}"),
                        label: info.human_name().to_owned(),
                        kind: DeviceKind::Webcam,
                    })
                })
                .collect(),
            Err(err) => {
                log::warn!("webcams: {err}");
                Vec::new()
            }
        }
    }

    fn display_list() -> Vec<DeviceInfo> {
        let monitors = match xcap::Monitor::all() {
            Ok(monitors) => monitors,
            Err(err) => {
                log::warn!("displays: {err}");
                return Vec::new();
            }
        };
        monitors
            .iter()
            .enumerate()
            .map(|(index, monitor)| {
                let label = monitor.name().unwrap_or_else(|_| format!("Display {index}"));
                DeviceInfo {
                    id: format!("display:{index}"),
                    label,
                    kind: DeviceKind::Display,
                }
            })
            .collect()
    }

    fn parse_index(id: &str, prefix: &str) -> Result<usize, DeviceError> {
        id.strip_prefix(prefix)
            .ok_or_else(|| DeviceError::Missing(id.to_owned()))?
            .parse()
            .map_err(|_| DeviceError::Missing(id.to_owned()))
    }

    fn to_mono_s16(bytes: &[u8], format: SampleFormat, channels: u16) -> Vec<i16> {
        let channels = channels.max(1) as usize;
        let interleaved = match format {
            SampleFormat::F32 => bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect::<Vec<f32>>(),
            SampleFormat::I16 => bytes
                .chunks_exact(2)
                .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32767.0)
                .collect::<Vec<f32>>(),
            SampleFormat::U16 => bytes
                .chunks_exact(2)
                .map(|chunk| (u16::from_le_bytes([chunk[0], chunk[1]]) as f32 - 32768.0) / 32768.0)
                .collect::<Vec<f32>>(),
            _ => return Vec::new(),
        };
        mix_mono(&interleaved, channels)
            .into_iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect()
    }

    fn mix_mono(samples: &[f32], channels: usize) -> Vec<f32> {
        if channels <= 1 {
            return samples.to_vec();
        }
        samples
            .chunks(channels)
            .filter(|frame| frame.len() == channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    }

    fn s16_bytes(samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    fn s16le_to_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32767.0)
            .collect()
    }

    fn pull_scaled<T: Copy>(dst: &mut [T], channels: usize, ring: &AudioRing, map: impl Fn(f32) -> T) {
        let channels = channels.max(1);
        let frames = dst.len() / channels;
        let mut mono = vec![0.0f32; frames];
        let missing = {
            let mut queue = ring.samples.lock().unwrap_or_else(|err| err.into_inner());
            let mut missing = false;
            for sample in &mut mono {
                match queue.pop_front() {
                    Some(value) => *sample = value,
                    None => {
                        missing = true;
                        *sample = 0.0;
                    }
                }
            }
            missing
        };
        for (frame, sample) in mono.iter().enumerate() {
            let mapped = map(*sample);
            for channel in 0..channels {
                dst[frame * channels + channel] = mapped;
            }
        }
        if missing {
            ring.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub use platform::DeviceSession;
