//! Who may talk to the tower over the network: paired devices, each with a
//! long random token the device keeps. The tower keeps only the token's
//! SHA-256 (`devices.json`, mode 0600), so a copy of that file can't be used
//! to call the API. A device is paired with a one-time code shown on the Mac.
//!
//! The daemon and `tower phone` both change `devices.json`: every change is
//! a whole read, change and write under a lock (`update`), so two writers
//! never undo each other (a revoked phone can't come back).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

/// A pairing code lasts this long.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong codes from one address before the code is void for it.
pub const CODE_TRIES: u32 = 5;
/// Wrong codes from all addresses before the code is void (a new one must be made on the Mac).
pub const CODE_TRIES_ALL: u32 = 30;

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
    /// Where replies to what this phone sends are heard: `phone` (only on
    /// the phone; the Mac stays silent) or `mac`.
    #[serde(default = "phone_route")]
    pub audio_route: String,
}

fn phone_route() -> String {
    "phone".into()
}

/// `phone` or `mac`.
pub fn valid_route(r: &str) -> bool {
    matches!(r, "phone" | "mac")
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<Device>,
}

impl Devices {
    pub fn path() -> PathBuf {
        crate::config::config_dir().join("devices.json")
    }

    /// What is on file: none when there is no file yet, an error when it
    /// can't be read whole (never "no devices" for a broken file).
    pub fn load_from(path: &Path) -> Result<Devices> {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("{} is damaged", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Devices::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Owner-only, written whole through a temporary file of its own.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        static N: AtomicU64 = AtomicU64::new(0);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let write = || -> Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            std::io::Write::write_all(&mut f, serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        };
        write()
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
            .with_context(|| format!("writing {}", path.display()))
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
            audio_route: phone_route(),
        };
        self.devices.push(d.clone());
        (d, token)
    }

    /// Sets where a device's replies are heard. False: no such device.
    pub fn set_route(&mut self, id: &str, route: &str) -> bool {
        match self.devices.iter_mut().find(|d| d.id == id) {
            Some(d) if valid_route(route) => {
                d.audio_route = route.to_string();
                true
            }
            _ => false,
        }
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

/// Every change to `devices.json` in this process waits here; other
/// processes (`tower phone`) wait on the lock file beside it.
static WRITING: Mutex<()> = Mutex::new(());

/// Changes `devices.json`: read, `change`, written back if it changed, all
/// under the lock. A damaged file is an error and stays as it is.
pub fn update<T>(path: &Path, change: impl FnOnce(&mut Devices) -> T) -> Result<T> {
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let _file = lock_file(path)?;
    let before = Devices::load_from(path)?;
    let mut d = before.clone();
    let out = change(&mut d);
    if d != before {
        d.save_to(path)?;
    }
    Ok(out)
}

/// An exclusive `flock` on `<path>.lock`, held until the file is dropped.
fn lock_file(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = path.with_extension("json.lock");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock)
        .with_context(|| format!("opening {}", lock.display()))?;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(f);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e).with_context(|| format!("locking {}", lock.display()));
        }
    }
}

/// `devices.json` as the daemon reads it on every call: the file as it is
/// now, or, while it can't be read, the last copy that could (a damaged
/// file doesn't unpair every phone).
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    last_good: Mutex<Option<Devices>>,
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path, last_good: Mutex::new(None) }
    }

    pub fn load(&self) -> Devices {
        let mut last = self.last_good.lock().unwrap_or_else(|e| e.into_inner());
        match Devices::load_from(&self.path) {
            Ok(d) => {
                *last = Some(d.clone());
                d
            }
            Err(e) => {
                eprintln!("[phone] {e:#}: using the last good copy");
                last.clone().unwrap_or_default()
            }
        }
    }

    /// `update` on this file.
    pub fn update<T>(&self, change: impl FnOnce(&mut Devices) -> T) -> Result<T> {
        update(&self.path, change)
    }
}

/// The pairing code on offer: one at a time, one use, a few minutes, a few
/// tries from each address (so one address can't burn it for the others).
#[derive(Debug, Default)]
pub struct Pairing {
    code: Option<(String, Instant)>,
    /// Wrong tries for this code, by address.
    tries: HashMap<IpAddr, u32>,
}

#[derive(Debug, PartialEq)]
pub enum PairError {
    /// No code on offer (none made, used, expired or burned), or none for
    /// this address any more.
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
        self.code = Some((code.clone(), Instant::now()));
        self.tries.clear();
        code
    }

    pub fn close(&mut self) {
        self.code = None;
        self.tries.clear();
    }

    /// When the code on offer expires, if one is.
    pub fn expires_in(&self) -> Option<Duration> {
        let (_, made) = self.code.as_ref()?;
        CODE_TTL.checked_sub(made.elapsed())
    }

    /// Takes the code (spaces and dashes don't matter) from `from`. Right: it's used up.
    pub fn take(&mut self, given: &str, from: IpAddr) -> Result<(), PairError> {
        self.take_at(given, from, Instant::now())
    }

    fn take_at(&mut self, given: &str, from: IpAddr, now: Instant) -> Result<(), PairError> {
        let Some((code, made)) = self.code.as_ref() else { return Err(PairError::NoCode) };
        if now.duration_since(*made) > CODE_TTL {
            self.close();
            return Err(PairError::NoCode);
        }
        if self.tries.get(&from).is_some_and(|t| *t >= CODE_TRIES) {
            return Err(PairError::NoCode);
        }
        let given: String = given.chars().filter(|c| c.is_ascii_digit()).collect();
        if same(given.as_bytes(), code.as_bytes()) {
            self.close();
            return Ok(());
        }
        let tries = self.tries.entry(from).or_insert(0);
        *tries += 1;
        let left = CODE_TRIES.saturating_sub(*tries);
        if self.tries.values().sum::<u32>() >= CODE_TRIES_ALL {
            self.close();
            return Err(PairError::Wrong { left: 0 });
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
    fn audio_route_defaults_to_the_phone_and_is_kept() {
        let mut d = Devices::default();
        let (dev, _) = d.add("Pixel", 1);
        assert_eq!(dev.audio_route, "phone");
        assert!(d.set_route(&dev.id, "mac"));
        assert!(!d.set_route(&dev.id, "speaker"), "only phone or mac");
        assert!(!d.set_route("d_nope", "mac"));
        assert_eq!(d.devices[0].audio_route, "mac");
        // A devices.json from before the setting reads as `phone`.
        let old: Devices = serde_json::from_str(r#"{"devices":[{"id":"d_1","name":"P","token_sha256":"x","created_ms":1}]}"#).unwrap();
        assert_eq!(old.devices[0].audio_route, "phone");
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
        let back = Devices::load_from(&path).unwrap();
        assert_eq!(back, d);
        assert!(back.find(&token).is_some());
        let leftovers: Vec<_> =
            std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_devices_file_is_an_error_not_an_empty_list() {
        let dir = std::env::temp_dir().join(format!("tower-devices-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        assert_eq!(Devices::load_from(&path).unwrap(), Devices::default(), "no file yet: no devices");
        let store = Store::new(path.clone());
        let token = store.update(|d| d.add("Pixel", 1).1).unwrap();
        assert!(store.load().find(&token).is_some());
        // Cut short, as a writer that didn't use the temporary file would leave it.
        let whole = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();
        assert!(Devices::load_from(&path).is_err());
        assert!(store.load().find(&token).is_some(), "the last good copy still lets the phone in");
        assert!(store.update(|d| d.add("Other", 2)).is_err(), "no change is written over a damaged file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), whole[..whole.len() / 2], "the damaged file is left for the user");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changes_from_many_writers_are_all_kept() {
        let dir = std::env::temp_dir().join(format!("tower-devices-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        let (first, _) = update(&path, |d| d.add("first", 1)).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    update(&path, |d| d.add(&format!("p{i}"), 1)).unwrap();
                })
            })
            .collect();
        // A revoke racing a `seen` that read before it: the device stays gone.
        update(&path, |d| d.revoke(&first.id)).unwrap();
        update(&path, |d| {
            if let Some(x) = d.devices.iter_mut().find(|x| x.id == first.id) {
                x.last_seen_ms = 99;
            }
        })
        .unwrap();
        for t in threads {
            t.join().unwrap();
        }
        let d = Devices::load_from(&path).unwrap();
        assert_eq!(d.devices.len(), 8, "every pairing is on file");
        assert!(d.devices.iter().all(|x| x.id != first.id), "the revoked device didn't come back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const A: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 20));
    const B: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 21));

    #[test]
    fn pairing_codes_are_one_time() {
        let mut p = Pairing::default();
        assert_eq!(p.take("12345678", A), Err(PairError::NoCode), "no code on offer");
        let code = p.open();
        assert_eq!(code.len(), 8);
        assert!(p.expires_in().is_some());
        let spaced = format!("{} {}", &code[..4], &code[4..]);
        assert_eq!(p.take(&spaced, A), Ok(()));
        assert_eq!(p.take(&code, A), Err(PairError::NoCode), "used up");
    }

    #[test]
    fn pairing_codes_burn_after_wrong_tries_and_expire() {
        let mut p = Pairing::default();
        let code = p.open();
        let wrong = if code == "00000000" { "11111111" } else { "00000000" };
        for left in (0..CODE_TRIES).rev() {
            assert_eq!(p.take(wrong, A), Err(PairError::Wrong { left }));
        }
        assert_eq!(p.take(&code, A), Err(PairError::NoCode), "burned for that address: even the right code fails there");
        assert_eq!(p.take(&code, B), Ok(()), "another address still pairs");

        let code = p.open();
        let later = Instant::now() + CODE_TTL + Duration::from_secs(1);
        assert_eq!(p.take_at(&code, A, later), Err(PairError::NoCode), "expired");
        // A new code replaces the old one.
        let first = p.open();
        let second = p.open();
        if first != second {
            assert!(matches!(p.take(&first, A), Err(PairError::Wrong { .. })));
        }
        assert_eq!(p.take(&second, A), Ok(()));
    }

    #[test]
    fn many_addresses_burn_the_code_at_last() {
        let mut p = Pairing::default();
        let code = p.open();
        let wrong = if code == "00000000" { "11111111" } else { "00000000" };
        let mut n = 0;
        'all: for host in 1..=255u8 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, host));
            for _ in 0..CODE_TRIES {
                n += 1;
                if p.take(wrong, ip) == Err(PairError::Wrong { left: 0 }) && n == CODE_TRIES_ALL {
                    break 'all;
                }
            }
        }
        assert_eq!(n, CODE_TRIES_ALL);
        assert_eq!(p.take(&code, B), Err(PairError::NoCode), "void for everyone");
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
