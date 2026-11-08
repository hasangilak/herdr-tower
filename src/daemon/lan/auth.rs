//! Who may talk to the tower over the network: paired devices, each with a
//! long random token the device keeps. The tower keeps only the token's
//! SHA-256 (`devices.json`, mode 0600), so a copy of that file can't be used
//! to call the API. A device is paired with a one-time code shown on the Mac.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

/// A pairing code lasts this long.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong codes before the code is void (a new one must be made on the Mac).
pub const CODE_TRIES: u32 = 5;

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    SystemRandom::new().fill(&mut b).expect("the system's random source");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

/// Equal without saying, by how long it takes, where they differ.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// A new device token: `htk_` and 256 random bits.
pub fn new_token() -> String {
    format!("htk_{}", base64url(&random_bytes::<32>()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// SHA-256 of its token, hex.
    pub token_sha256: String,
    pub created_ms: u64,
    #[serde(default)]
    pub last_seen_ms: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<Device>,
}

impl Devices {
    pub fn path() -> PathBuf {
        crate::config::config_dir().join("devices.json")
    }

    pub fn load_from(path: &Path) -> Devices {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Owner-only, written whole through a temporary file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        std::io::Write::write_all(&mut f, serde_json::to_string_pretty(self)?.as_bytes())?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The device this token belongs to.
    pub fn find(&self, token: &str) -> Option<&Device> {
        if !token.starts_with("htk_") || token.len() < 40 {
            return None;
        }
        let h = sha256_hex(token.as_bytes());
        // Every entry is compared, so the time doesn't depend on which matched.
        let mut found = None;
        for d in &self.devices {
            if same(d.token_sha256.as_bytes(), h.as_bytes()) {
                found = Some(d);
            }
        }
        found
    }

    /// Adds a device; returns it and its token (shown to the device once, never kept).
    pub fn add(&mut self, name: &str, now_ms: u64) -> (Device, String) {
        let token = new_token();
        let name: String = name.chars().filter(|c| !c.is_control()).take(60).collect();
        let d = Device {
            id: format!("d_{}", hex(&random_bytes::<4>())),
            name: if name.trim().is_empty() { "phone".into() } else { name.trim().to_string() },
            token_sha256: sha256_hex(token.as_bytes()),
            created_ms: now_ms,
            last_seen_ms: now_ms,
        };
        self.devices.push(d.clone());
        (d, token)
    }

    /// Removes a device by id, or every device for `all`. How many went.
    pub fn revoke(&mut self, id: &str) -> usize {
        let before = self.devices.len();
        if id == "all" {
            self.devices.clear();
        } else {
            self.devices.retain(|d| d.id != id);
        }
        before - self.devices.len()
    }
}

/// The pairing code on offer: one at a time, one use, a few minutes, a few tries.
#[derive(Debug, Default)]
pub struct Pairing {
    code: Option<(String, Instant, u32)>,
}

#[derive(Debug, PartialEq)]
pub enum PairError {
    /// No code on offer (none made, used, expired or burned).
    NoCode,
    Wrong {
        left: u32,
    },
}

impl Pairing {
    /// A new code (8 digits), replacing any other.
    pub fn open(&mut self) -> String {
        let n = u64::from_le_bytes(random_bytes::<8>()) % 100_000_000;
        let code = format!("{n:08}");
        self.code = Some((code.clone(), Instant::now(), 0));
        code
    }

    pub fn close(&mut self) {
        self.code = None;
    }

    /// When the code on offer expires, if one is.
    pub fn expires_in(&self) -> Option<Duration> {
        let (_, made, _) = self.code.as_ref()?;
        CODE_TTL.checked_sub(made.elapsed())
    }

    /// Takes the code (spaces and dashes don't matter). Right: it's used up.
    pub fn take(&mut self, given: &str) -> Result<(), PairError> {
        self.take_at(given, Instant::now())
    }

    fn take_at(&mut self, given: &str, now: Instant) -> Result<(), PairError> {
        let Some((code, made, tries)) = self.code.as_mut() else { return Err(PairError::NoCode) };
        if now.duration_since(*made) > CODE_TTL {
            self.code = None;
            return Err(PairError::NoCode);
        }
        let given: String = given.chars().filter(|c| c.is_ascii_digit()).collect();
        if same(given.as_bytes(), code.as_bytes()) {
            self.code = None;
            return Ok(());
        }
        *tries += 1;
        let left = CODE_TRIES.saturating_sub(*tries);
        if left == 0 {
            self.code = None;
        }
        Err(PairError::Wrong { left })
    }
}

/// The bearer token in an `Authorization` header value.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let h = header?.trim();
    let (scheme, token) = h.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

/// Checks a request's token against the devices on file.
pub fn authorize(devices: &Devices, header: Option<&str>) -> Result<Device> {
    let Some(token) = bearer(header) else { bail!("missing bearer token") };
    devices.find(token).cloned().ok_or_else(|| anyhow::anyhow!("unknown or revoked token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_and_kept_hashed() {
        let mut d = Devices::default();
        let (dev, token) = d.add("Pixel", 1);
        assert!(token.starts_with("htk_") && token.len() == 4 + 43, "{token}");
        assert_ne!(token, d.add("Other", 2).1);
        assert_eq!(dev.token_sha256, sha256_hex(token.as_bytes()));
        let file = serde_json::to_string(&d).unwrap();
        assert!(!file.contains(&token), "the token itself is never stored");
        assert_eq!(d.find(&token).map(|x| x.id.clone()), Some(dev.id.clone()));
    }

    #[test]
    fn every_call_needs_a_valid_token() {
        let mut d = Devices::default();
        let (dev, token) = d.add("Pixel", 1);
        assert_eq!(authorize(&d, Some(&format!("Bearer {token}"))).unwrap().id, dev.id);
        assert_eq!(authorize(&d, Some(&format!("bearer  {token} "))).unwrap().id, dev.id);
        for bad in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some(token.as_str()),
            Some("Basic abc"),
            Some("Bearer htk_wrongwrongwrongwrongwrongwrongwrongwrongwro"),
        ] {
            assert!(authorize(&d, bad).is_err(), "{bad:?}");
        }
        // The stored hash is not a token.
        assert!(authorize(&d, Some(&format!("Bearer {}", dev.token_sha256))).is_err());
    }

    #[test]
    fn revoking_stops_a_token() {
        let mut d = Devices::default();
        let (a, ta) = d.add("A", 1);
        let (_, tb) = d.add("B", 1);
        assert_eq!(d.revoke(&a.id), 1);
        assert!(d.find(&ta).is_none());
        assert!(d.find(&tb).is_some());
        assert_eq!(d.revoke("d_nope"), 0);
        assert_eq!(d.revoke("all"), 1);
        assert!(d.find(&tb).is_none());
    }

    #[test]
    fn devices_file_is_owner_only_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tower-devices-{}", std::process::id()));
        let path = dir.join("devices.json");
        let mut d = Devices::default();
        let (_, token) = d.add("Pixel", 5);
        d.save_to(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let back = Devices::load_from(&path);
        assert_eq!(back, d);
        assert!(back.find(&token).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_codes_are_one_time() {
        let mut p = Pairing::default();
        assert_eq!(p.take("12345678"), Err(PairError::NoCode), "no code on offer");
        let code = p.open();
        assert_eq!(code.len(), 8);
        assert!(p.expires_in().is_some());
        let spaced = format!("{} {}", &code[..4], &code[4..]);
        assert_eq!(p.take(&spaced), Ok(()));
        assert_eq!(p.take(&code), Err(PairError::NoCode), "used up");
    }

    #[test]
    fn pairing_codes_burn_after_wrong_tries_and_expire() {
        let mut p = Pairing::default();
        let code = p.open();
        let wrong = if code == "00000000" { "11111111" } else { "00000000" };
        for left in (0..CODE_TRIES).rev() {
            assert_eq!(p.take(wrong), Err(PairError::Wrong { left }));
        }
        assert_eq!(p.take(&code), Err(PairError::NoCode), "burned: even the right code fails now");

        let code = p.open();
        let later = Instant::now() + CODE_TTL + Duration::from_secs(1);
        assert_eq!(p.take_at(&code, later), Err(PairError::NoCode), "expired");
        // A new code replaces the old one.
        let first = p.open();
        let second = p.open();
        if first != second {
            assert!(matches!(p.take(&first), Err(PairError::Wrong { .. })));
        }
        assert_eq!(p.take(&second), Ok(()));
    }

    #[test]
    fn helpers() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(bearer(Some("Bearer x")), Some("x"));
        assert_eq!(bearer(Some("Token x")), None);
    }
}
