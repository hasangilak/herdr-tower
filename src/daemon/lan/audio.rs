//! WAV both ways: the phone uploads what it recorded, the tower sends what it says.

use anyhow::{Result, bail};

/// Mono 16-bit PCM WAV.
pub fn wav_encode(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    w
}

/// A WAV file's header, read without decoding its audio.
#[derive(Debug)]
pub struct Wav<'a> {
    /// 1: PCM, 3: float.
    format: u16,
    channels: usize,
    pub rate: u32,
    data: &'a [u8],
}

impl<'a> Wav<'a> {
    /// PCM 16-bit or 32-bit float, one or two channels, 8 to 96 kHz.
    pub fn parse(bytes: &'a [u8]) -> Result<Wav<'a>> {
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            bail!("not a WAV file");
        }
        let (mut fmt, mut data) = (None, None);
        let mut i = 12;
        while i + 8 <= bytes.len() {
            let id = &bytes[i..i + 4];
            let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into()?) as usize;
            let body = &bytes[i + 8..(i + 8).saturating_add(len).min(bytes.len())];
            match id {
                b"fmt " => fmt = Some(body),
                b"data" => data = Some(body),
                _ => {}
            }
            i = i.saturating_add(8 + len + (len & 1));
        }
        let (Some(fmt), Some(data)) = (fmt, data) else { bail!("WAV without fmt or data") };
        if fmt.len() < 16 {
            bail!("short WAV fmt");
        }
        let format = u16::from_le_bytes([fmt[0], fmt[1]]);
        let channels = u16::from_le_bytes([fmt[2], fmt[3]]) as usize;
        let rate = u32::from_le_bytes(fmt[4..8].try_into()?);
        let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
        if !(1..=2).contains(&channels) || !(8_000..=96_000).contains(&rate) {
            bail!("WAV must be mono or stereo, 8 to 96 kHz");
        }
        if !matches!((format, bits), (1, 16) | (3, 32)) {
            bail!("WAV must be 16-bit PCM or 32-bit float");
        }
        Ok(Wav { format, channels, rate, data })
    }

    fn frame_bytes(&self) -> usize {
        self.channels * if self.format == 1 { 2 } else { 4 }
    }

    /// How long it plays, from the header and the data's size.
    pub fn seconds(&self) -> f32 {
        (self.data.len() / self.frame_bytes()) as f32 / self.rate as f32
    }

    /// The audio as mono samples.
    pub fn decode(&self) -> Vec<f32> {
        let frames: Vec<f32> = match self.format {
            1 => self.data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect(),
            _ => self.data.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
        };
        if self.channels == 1 { frames } else { frames.chunks_exact(2).map(|c| (c[0] + c[1]) / 2.0).collect() }
    }
}

/// A WAV file's audio as mono samples, and its sample rate.
#[cfg(test)]
pub fn wav_decode(bytes: &[u8]) -> Result<(Vec<f32>, u32)> {
    let w = Wav::parse(bytes)?;
    Ok((w.decode(), w.rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let s: Vec<f32> = (0..1600).map(|i| (i as f32 / 50.0).sin() * 0.5).collect();
        let w = wav_encode(&s, 16_000);
        assert_eq!(w.len(), 44 + 3200);
        let (back, rate) = wav_decode(&w).unwrap();
        assert_eq!((rate, back.len()), (16_000, 1600));
        assert!(back.iter().zip(&s).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn mixes_stereo_and_refuses_junk() {
        let mut w = wav_encode(&[0.5, -0.5, 0.25, 0.25], 48_000);
        w[22] = 2; // two channels: the 4 samples are 2 frames
        let (m, _) = wav_decode(&w).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m[0].abs() < 1e-3 && (m[1] - 0.25).abs() < 1e-3);
        assert!(wav_decode(b"hello").is_err());
        assert!(wav_decode(&w[..30]).is_err());
    }

    #[test]
    fn its_length_is_known_before_decoding() {
        let w = wav_encode(&vec![0.0; 16_000 * 3], 16_000);
        assert!((Wav::parse(&w).unwrap().seconds() - 3.0).abs() < 1e-3);
        let mut stereo = wav_encode(&vec![0.0; 48_000 * 2], 48_000);
        stereo[22] = 2;
        assert!((Wav::parse(&stereo).unwrap().seconds() - 1.0).abs() < 1e-3);
        // A data chunk that claims more than is there counts what is there.
        let mut cut = wav_encode(&vec![0.0; 16_000], 16_000);
        cut.truncate(44 + 16_000);
        assert!((Wav::parse(&cut).unwrap().seconds() - 0.5).abs() < 1e-3);
        let mut eight = wav_encode(&[0.0; 10], 16_000);
        eight[34] = 8;
        assert!(Wav::parse(&eight).is_err(), "8-bit is refused before decoding");
    }
}
