//! Bus that connects capture sources to processors and readers.
//!
//! `publish` never blocks. A subscriber that falls behind drops samples.

use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::sample::{MediaSample, SourceKind, Stage};

const CAPACITY: usize = 8;

struct Subscription {
    source: Option<SourceKind>,
    stage: Stage,
    tx: SyncSender<MediaSample>,
}

struct Inner {
    subs: Vec<Subscription>,
    latest: HashMap<(SourceKind, Stage), MediaSample>,
}

/// Shared publish/subscribe bus. Clones share the same samples.
#[derive(Clone)]
pub struct DataStream {
    inner: Arc<Mutex<Inner>>,
}

impl Default for DataStream {
    fn default() -> Self {
        Self::new()
    }
}

impl DataStream {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                subs: Vec::new(),
                latest: HashMap::new(),
            })),
        }
    }

    pub fn publish(&self, sample: MediaSample) {
        let mut inner = lock(&self.inner);
        inner.latest.insert((sample.source, sample.stage), sample.clone());
        inner.subs.retain(|sub| {
            let source_ok = sub.source.map(|kind| kind == sample.source).unwrap_or(true);
            if !source_ok || sub.stage != sample.stage {
                return true;
            }
            match sub.tx.try_send(sample.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => true,
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// `source == None` receives every source on that stage.
    pub fn subscribe(&self, source: Option<SourceKind>, stage: Stage) -> Receiver<MediaSample> {
        let (tx, rx) = sync_channel(CAPACITY);
        lock(&self.inner).subs.push(Subscription { source, stage, tx });
        rx
    }

    /// Newest sample for this source and stage, if one has been published.
    pub fn latest(&self, source: SourceKind, stage: Stage) -> Option<MediaSample> {
        lock(&self.inner).latest.get(&(source, stage)).cloned()
    }
}

fn lock(mutex: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::MediaSample;
    use std::time::Duration;

    fn tone(seq: u64) -> MediaSample {
        MediaSample::audio(seq, 16_000, vec![seq as u8, 0])
    }

    #[test]
    fn fanout_reaches_every_subscriber() {
        let bus = DataStream::new();
        let left = bus.subscribe(Some(SourceKind::AudioIn), Stage::Raw);
        let right = bus.subscribe(None, Stage::Raw);
        bus.publish(tone(1));
        let a = left.recv_timeout(Duration::from_secs(1)).expect("left");
        let b = right.recv_timeout(Duration::from_secs(1)).expect("right");
        assert_eq!(a.seq, 1);
        assert_eq!(b.seq, 1);
        assert_eq!(bus.latest(SourceKind::AudioIn, Stage::Raw).unwrap().seq, 1);
    }

    #[test]
    fn a_full_subscriber_does_not_block_a_later_one() {
        let bus = DataStream::new();
        let slow = bus.subscribe(Some(SourceKind::AudioIn), Stage::Raw);
        for seq in 0..(CAPACITY as u64 + 5) {
            bus.publish(tone(seq));
        }
        let fresh = bus.subscribe(Some(SourceKind::AudioIn), Stage::Raw);
        bus.publish(tone(100));
        let got = fresh.recv_timeout(Duration::from_secs(1)).expect("fresh");
        assert_eq!(got.seq, 100);
        let mut kept = 0;
        while slow.try_recv().is_ok() {
            kept += 1;
        }
        assert!(kept <= CAPACITY, "slow subscriber kept {kept}");
        assert!(kept > 0);
    }

    #[test]
    fn stage_filter_skips_other_stages() {
        let bus = DataStream::new();
        let raw = bus.subscribe(Some(SourceKind::Display), Stage::Raw);
        let mut compressed = MediaSample::video(SourceKind::Display, 3, 1, 1, vec![9]);
        compressed.stage = Stage::Compressed;
        bus.publish(compressed);
        assert!(raw.try_recv().is_err());
        assert!(bus.latest(SourceKind::Display, Stage::Raw).is_none());
        assert_eq!(bus.latest(SourceKind::Display, Stage::Compressed).unwrap().seq, 3);
    }
}
