//! Who is listening over the network, and what the tower says to them.
//!
//! Each open `GET /v1/events` stream is a subscriber. One opened with
//! `voice=1` makes its phone the tower's voice: while at least one is open,
//! speech goes to the phones (as WAV clips announced by an `audio` event and
//! fetched from `/v1/audio/<id>`) and the Mac stays silent; when the last
//! closes, the Mac speaks again.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Clips kept for the phone to fetch.
const CLIPS: usize = 200;
const CLIP_AGE: Duration = Duration::from_secs(15 * 60);

pub struct Out {
    pub event: String,
    pub data: Value,
}

struct Sub {
    id: u64,
    device: String,
    voice: bool,
    tx: Sender<Out>,
}

struct Clip {
    id: String,
    at: Instant,
    wav: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    subs: Vec<Sub>,
    clips: VecDeque<Clip>,
    next_sub: u64,
    next_clip: u64,
}

#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
}

impl Hub {
    /// A new stream. Returns its id and where its events arrive.
    pub fn subscribe(&self, device: &str, voice: bool) -> (u64, Receiver<Out>) {
        let (tx, rx) = channel();
        let mut i = self.inner.lock().unwrap();
        i.next_sub += 1;
        let id = i.next_sub;
        let was = i.subs.iter().any(|s| s.voice);
        i.subs.push(Sub { id, device: device.to_string(), voice, tx });
        if voice && !was {
            eprintln!("[phone] {device} is the tower's voice now");
        }
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        let mut i = self.inner.lock().unwrap();
        let was = i.subs.iter().any(|s| s.voice);
        i.subs.retain(|s| s.id != id);
        if was && !i.subs.iter().any(|s| s.voice) {
            eprintln!("[phone] no phone is the voice: back to the Mac");
        }
    }

    /// Ends every stream of a device (it was revoked).
    pub fn drop_device(&self, device: &str) {
        self.inner.lock().unwrap().subs.retain(|s| s.device != device);
    }

    /// A phone is the voice: the Mac stays silent.
    pub fn voice(&self) -> bool {
        self.inner.lock().unwrap().subs.iter().any(|s| s.voice)
    }

    /// (streams, voice streams)
    pub fn counts(&self) -> (usize, usize) {
        let i = self.inner.lock().unwrap();
        (i.subs.len(), i.subs.iter().filter(|s| s.voice).count())
    }

    fn send(&self, only_voice: bool, event: &str, data: &Value) {
        let mut i = self.inner.lock().unwrap();
        // A stream whose reader is gone drops out here.
        i.subs.retain(|s| (only_voice && !s.voice) || s.tx.send(Out { event: event.to_string(), data: data.clone() }).is_ok());
    }

    /// To every stream (the state and history come by polling; this is for news all of them need).
    #[allow(dead_code)]
    pub fn broadcast(&self, event: &str, data: Value) {
        self.send(false, event, &data);
    }

    pub fn to_voice(&self, event: &str, data: Value) {
        self.send(true, event, &data);
    }

    /// Speech for the phones: kept as WAV, announced by an `audio` event. Returns its id.
    pub fn push_audio(&self, kind: &str, text: &str, samples: &[f32], rate: u32) -> String {
        let wav = super::audio::wav_encode(samples, rate);
        let ms = (samples.len() as u64 * 1000) / rate.max(1) as u64;
        let id = {
            let mut i = self.inner.lock().unwrap();
            i.next_clip += 1;
            let id = format!("a{}", i.next_clip);
            i.clips.push_back(Clip { id: id.clone(), at: Instant::now(), wav });
            while i.clips.len() > CLIPS || i.clips.front().is_some_and(|c| c.at.elapsed() > CLIP_AGE) {
                i.clips.pop_front();
            }
            id
        };
        self.to_voice("audio", json!({ "id": id, "kind": kind, "text": text, "ms": ms, "url": format!("/v1/audio/{id}") }));
        id
    }

    pub fn clip(&self, id: &str) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().clips.iter().find(|c| c.id == id).map(|c| c.wav.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_voice_follows_voice_streams() {
        let h = Hub::default();
        assert!(!h.voice());
        let (a, rx_a) = h.subscribe("d_1", false);
        assert!(!h.voice(), "a plain stream doesn't take the voice");
        let (b, rx_b) = h.subscribe("d_1", true);
        assert!(h.voice());
        let id = h.push_audio("reply", "Hello.", &[0.0; 2400], 24_000);
        let got = rx_b.try_recv().unwrap();
        assert_eq!((got.event.as_str(), got.data["id"].as_str(), got.data["ms"].as_u64()), ("audio", Some(id.as_str()), Some(100)));
        assert!(rx_a.try_recv().is_err(), "audio goes to voice streams only");
        assert!(h.clip(&id).is_some_and(|w| w.starts_with(b"RIFF")));
        h.broadcast("state", json!({}));
        assert_eq!(rx_a.try_recv().unwrap().event, "state");
        h.unsubscribe(b);
        assert!(!h.voice(), "back to the Mac");
        drop(rx_a);
        h.broadcast("state", json!({}));
        assert_eq!(h.counts(), (0, 0), "a closed stream drops out");
        let _ = a;
    }

    #[test]
    fn revoked_devices_lose_their_streams() {
        let h = Hub::default();
        let (_, rx) = h.subscribe("d_1", true);
        h.subscribe("d_2", false);
        h.drop_device("d_1");
        assert!(!h.voice());
        assert!(rx.recv().is_err(), "its stream ends");
        assert_eq!(h.counts(), (1, 0));
    }
}
