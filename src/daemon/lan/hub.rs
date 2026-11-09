//! Who is listening over the network, and what the tower says to them.
//!
//! Each open `GET /v1/events` stream is a subscriber, of a device whose
//! replies go either to the phone (`route` `phone`, the default) or to the
//! Mac (`mac`); see `auth::Device::audio_route`.
//!
//! - A turn that started on a phone with route `phone`: its replies and its
//!   "Okay." go to that phone only (`push_audio` with `to`), on every stream
//!   it has open; the Mac stays silent. With none open, the clips wait
//!   15 minutes (`clips_for`, `GET /v1/audio`).
//! - Anything else (turns started on the Mac, alerts): to the phones that
//!   have a `voice=1` stream open with route `phone` (the Mac stays silent),
//!   or else on the Mac, as before.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Clips kept for the phone to fetch.
const CLIPS: usize = 200;
pub const CLIP_AGE: Duration = Duration::from_secs(15 * 60);

pub struct Out {
    pub event: String,
    pub data: Value,
}

struct Sub {
    id: u64,
    device: String,
    voice: bool,
    /// Its device hears replies on the phone (route `phone`).
    on_phone: bool,
    tx: Sender<Out>,
}

struct Clip {
    id: String,
    at: Instant,
    /// The device it was for (None: the voice streams).
    to: Option<String>,
    meta: Value,
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
    /// A new stream. `on_phone`: its device's route is `phone`. Returns its id and where its events arrive.
    pub fn subscribe(&self, device: &str, voice: bool, on_phone: bool) -> (u64, Receiver<Out>) {
        let (tx, rx) = channel();
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.next_sub += 1;
        let id = i.next_sub;
        let was = i.subs.iter().any(|s| s.voice && s.on_phone);
        i.subs.push(Sub { id, device: device.to_string(), voice, on_phone, tx });
        if voice && on_phone && !was {
            eprintln!("[phone] {device} is the tower's voice now");
        }
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let was = i.subs.iter().any(|s| s.voice && s.on_phone);
        i.subs.retain(|s| s.id != id);
        if was && !i.subs.iter().any(|s| s.voice && s.on_phone) {
            eprintln!("[phone] no phone is the voice: back to the Mac");
        }
    }

    /// A device's route changed (`PUT /v1/device/audio`, the panel).
    pub fn set_route(&self, device: &str, on_phone: bool) {
        for s in self.inner.lock().unwrap_or_else(|e| e.into_inner()).subs.iter_mut().filter(|s| s.device == device) {
            s.on_phone = on_phone;
        }
    }

    /// Ends every stream of a device (it was revoked).
    pub fn drop_device(&self, device: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).subs.retain(|s| s.device != device);
    }

    /// Ends every stream (the API was turned off): no phone is the voice.
    pub fn drop_all(&self) {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let was = i.subs.iter().any(|s| s.voice && s.on_phone);
        i.subs.clear();
        if was {
            eprintln!("[phone] no phone is the voice: back to the Mac");
        }
    }

    /// Forgets the clips waiting to be fetched (the tower was muted: what it
    /// was going to say isn't played later).
    pub fn drop_clips(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clips.clear();
    }

    /// A phone is the voice for everything not sent from a phone: the Mac stays silent.
    pub fn voice(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).subs.iter().any(|s| s.voice && s.on_phone)
    }

    /// (streams, voice streams that have the voice)
    pub fn counts(&self) -> (usize, usize) {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (i.subs.len(), i.subs.iter().filter(|s| s.voice && s.on_phone).count())
    }

    fn send(&self, to: impl Fn(&Sub) -> bool, event: &str, data: &Value) {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // A stream whose reader is gone drops out here.
        i.subs.retain(|s| !to(s) || s.tx.send(Out { event: event.to_string(), data: data.clone() }).is_ok());
    }

    /// To every stream, voice or not (`hush`, `pause`, `resume`: whatever plays stops).
    pub fn broadcast(&self, event: &str, data: Value) {
        self.send(|_| true, event, &data);
    }

    /// Speech for the phones: kept as WAV, announced by an `audio` event.
    /// `to`: the device a phone turn's reply is for (every stream it has
    /// open); None: the voice streams. Returns its id.
    pub fn push_audio(&self, kind: &str, text: &str, samples: &[f32], rate: u32, to: Option<&str>) -> String {
        let wav = super::audio::wav_encode(samples, rate);
        let ms = (samples.len() as u64 * 1000) / rate.max(1) as u64;
        let (id, meta) = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.next_clip += 1;
            // In order, and not guessable from another phone's.
            let id = format!("a{}-{}", i.next_clip, super::auth::hex(&super::auth::random_bytes::<8>()));
            let at_ms = super::super::state::now_ms();
            let meta = json!({ "id": id, "kind": kind, "text": text, "ms": ms, "url": format!("/v1/audio/{id}"), "at_ms": at_ms });
            i.clips.push_back(Clip { id: id.clone(), at: Instant::now(), to: to.map(String::from), meta: meta.clone(), wav });
            while i.clips.len() > CLIPS || i.clips.front().is_some_and(|c| c.at.elapsed() > CLIP_AGE) {
                i.clips.pop_front();
            }
            (id, meta)
        };
        match to {
            Some(d) => self.send(|s| s.device == d, "audio", &meta),
            None => self.send(|s| s.voice && s.on_phone, "audio", &meta),
        }
        id
    }

    /// A clip's WAV, for `device`: one meant for it, or for the voice streams.
    pub fn clip(&self, id: &str, device: &str) -> Option<Vec<u8>> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.clips.iter().find(|c| c.id == id && c.to.as_deref().is_none_or(|d| d == device)).map(|c| c.wav.clone())
    }

    /// The clips a device may have missed (its own, and the voice's), oldest
    /// first, after clip `after` (its id) when given.
    pub fn clips_for(&self, device: &str, after: Option<&str>) -> Vec<Value> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let start = after.and_then(|a| i.clips.iter().position(|c| c.id == a)).map(|p| p + 1).unwrap_or(0);
        i.clips
            .iter()
            .skip(start)
            .filter(|c| c.at.elapsed() <= CLIP_AGE && c.to.as_deref().is_none_or(|d| d == device))
            .map(|c| c.meta.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_voice_follows_voice_streams_of_phone_route_devices() {
        let h = Hub::default();
        assert!(!h.voice());
        let (_, rx_a) = h.subscribe("d_1", false, true);
        assert!(!h.voice(), "a plain stream doesn't take the voice");
        let (_, _rx_m) = h.subscribe("d_m", true, false);
        assert!(!h.voice(), "a voice stream of a route-mac phone doesn't either");
        let (b, rx_b) = h.subscribe("d_1", true, true);
        assert!(h.voice());
        let id = h.push_audio("reply", "Hello.", &[0.0; 2400], 24_000, None);
        let got = rx_b.try_recv().unwrap();
        assert_eq!((got.event.as_str(), got.data["id"].as_str(), got.data["ms"].as_u64()), ("audio", Some(id.as_str()), Some(100)));
        assert!(rx_a.try_recv().is_err(), "the voice's audio goes to voice streams only");
        assert!(h.clip(&id, "d_1").is_some_and(|w| w.starts_with(b"RIFF")));
        assert!(h.clip(&id, "d_m").is_some(), "the voice's clips are for any phone");
        h.broadcast("hush", json!({}));
        assert_eq!(rx_a.try_recv().unwrap().event, "hush");
        h.unsubscribe(b);
        assert!(!h.voice(), "back to the Mac");
        h.set_route("d_m", true);
        assert!(h.voice(), "switching a voice stream's phone to route phone gives it the voice");
    }

    #[test]
    fn a_phone_turn_goes_to_that_phone_on_any_stream() {
        let h = Hub::default();
        let (_, rx_1) = h.subscribe("d_1", false, true);
        let (_, rx_2) = h.subscribe("d_2", true, true);
        let id = h.push_audio("ack", "Okay.", &[0.0; 240], 24_000, Some("d_1"));
        assert_eq!(rx_1.try_recv().unwrap().data["id"].as_str(), Some(id.as_str()), "its plain stream gets it");
        assert!(rx_2.try_recv().is_err(), "another phone's voice stream doesn't");
        // Missed clips: the device's own and the voice's, never another phone's.
        let mine = h.push_audio("reply", "Done.", &[0.0; 240], 24_000, Some("d_1"));
        let theirs = h.push_audio("reply", "Not yours.", &[0.0; 240], 24_000, Some("d_2"));
        let voice = h.push_audio("alert", "News.", &[0.0; 240], 24_000, None);
        let ids = |v: Vec<Value>| v.iter().map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(h.clips_for("d_1", None)), [id.clone(), mine.clone(), voice.clone()]);
        assert_eq!(ids(h.clips_for("d_1", Some(&mine))), [voice.clone()]);
        assert_eq!(ids(h.clips_for("d_2", None)), [theirs.clone(), voice]);
        // Another phone's clip can't be fetched, nor its id guessed from one's own.
        assert!(h.clip(&theirs, "d_1").is_none());
        assert!(h.clip(&theirs, "d_2").is_some());
        assert!(h.clip(&mine, "d_1").is_some());
        let n: u64 = mine[1..].split('-').next().unwrap().parse().unwrap();
        assert!(h.clip(&format!("a{}", n + 1), "d_1").is_none() && theirs.starts_with(&format!("a{}-", n + 1)));
        assert!(theirs.len() > 10, "{theirs}");
    }

    #[test]
    fn muting_drops_waiting_clips_and_stopping_ends_every_stream() {
        let h = Hub::default();
        let (_, rx) = h.subscribe("d_1", true, true);
        let (_, rx2) = h.subscribe("d_2", false, true);
        let id = h.push_audio("reply", "Later.", &[0.0; 240], 24_000, Some("d_1"));
        h.drop_clips();
        assert!(h.clip(&id, "d_1").is_none() && h.clips_for("d_1", None).is_empty());
        h.drop_all();
        assert!(!h.voice());
        assert_eq!(h.counts(), (0, 0));
        assert!(rx.iter().all(|o| o.event == "audio"), "the stream ends after what it had");
        assert!(rx2.recv().is_err());
    }

    #[test]
    fn revoked_devices_lose_their_streams() {
        let h = Hub::default();
        let (_, rx) = h.subscribe("d_1", true, true);
        h.subscribe("d_2", false, true);
        h.drop_device("d_1");
        assert!(!h.voice());
        assert!(rx.recv().is_err(), "its stream ends");
        assert_eq!(h.counts(), (1, 0));
    }
}
