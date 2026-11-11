//! Bonjour (DNS-SD) for the phone API: while the LAN listener runs, this
//! Mac is advertised as `_herdr-tower._tcp` in `local.`, on the interface
//! that holds the listener's address, with the certificate's fingerprint in
//! its TXT record. A paired phone that can't reach any address it knows
//! (the Mac is on another network now) browses for the service and connects
//! to the one whose `fp` is the fingerprint it pinned at pairing, with the
//! same pin and token. The pin is what it trusts, not the advert: anyone can
//! advertise, nobody else can pass the pin.
//!
//! macOS's own DNS-SD API (`dns_sd.h`, in libSystem; mDNSResponder does the
//! work), behind `Advert`: registered while it lives, withdrawn when it is
//! dropped. A small thread reads mDNSResponder's replies (the name it was
//! registered under, renamed on a clash). Elsewhere nothing is advertised.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};

/// The service type the phone browses for.
pub const SERVICE: &str = "_herdr-tower._tcp";
/// Only the local network (multicast DNS), never a wide-area domain.
pub const DOMAIN: &str = "local.";
/// The TXT record's own version (`v`).
pub const TXT_VERSION: &str = "1";
/// The longest instance name DNS-SD takes, in bytes (one DNS label).
pub const MAX_NAME: usize = 63;
/// The longest string in a TXT record, in bytes (its length is one byte).
pub const MAX_TXT_STRING: usize = 255;
/// When this Mac has no name to give (it always has one on macOS).
const FALLBACK_NAME: &str = "Herdr Tower";

/// A network interface of this Mac: its name (`en0`) and its index (`if_nametoindex`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub index: u32,
}

/// What is advertised: the listener's address and port, the interface that
/// holds that address, the instance name and the fingerprint. Another one
/// (the Mac moved, the interface changed, the Mac was renamed) is advertised
/// again in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ad {
    pub ip: IpAddr,
    pub port: u16,
    pub interface: Interface,
    pub name: String,
    /// The TXT's `fp`: 64 lowercase hex digits.
    pub fp: String,
}

impl Ad {
    /// Its TXT record (`txt`).
    pub fn txt(&self) -> Result<Vec<u8>> {
        txt(&self.fp, &self.name)
    }
}

/// The certificate's SHA-256 as the TXT carries it: 64 lowercase hex
/// digits, without the colons, spaces or dashes it may be written with.
pub fn fp_hex(fp: &str) -> Option<String> {
    let hex: String = fp.chars().filter(|c| !matches!(c, ':' | ' ' | '-')).map(|c| c.to_ascii_lowercase()).collect();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// `s` cut to at most `max` bytes, at a character's edge.
fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The name to register: without control characters, trimmed, at most 63
/// bytes (cut at a character); never empty.
pub fn instance_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let name = cut(clean.trim(), MAX_NAME).trim_end();
    if name.is_empty() { FALLBACK_NAME.to_string() } else { name.to_string() }
}

/// The TXT record: `v=1`, `fp=<64 hex>`, `name=<the name>`, in that order,
/// each string its length in a byte and then its bytes (at most 255: a
/// longer name is cut at a character).
pub fn txt(fp: &str, name: &str) -> Result<Vec<u8>> {
    let fp = fp_hex(fp).with_context(|| format!("{fp:?} isn't a SHA-256 fingerprint"))?;
    let name = cut(name, MAX_TXT_STRING - "name=".len());
    let mut out = vec![];
    for (k, v) in [("v", TXT_VERSION), ("fp", fp.as_str()), ("name", name)] {
        let s = format!("{k}={v}");
        out.push(u8::try_from(s.len()).context("a TXT string over 255 bytes")?);
        out.extend_from_slice(s.as_bytes());
    }
    Ok(out)
}

/// A TXT record's strings as `key=value` pairs, in order (`None`: it isn't
/// one). What the phone does with what it resolves; here, for the tests.
#[cfg(test)]
pub fn txt_entries(txt: &[u8]) -> Option<Vec<(String, String)>> {
    let mut out = vec![];
    let mut rest = txt;
    while let Some((&n, tail)) = rest.split_first() {
        let s = std::str::from_utf8(tail.get(..n as usize)?).ok()?;
        let (k, v) = s.split_once('=').unwrap_or((s, ""));
        out.push((k.to_string(), v.to_string()));
        rest = &tail[n as usize..];
    }
    Some(out)
}

/// Every address on this Mac's interfaces that are up, with the
/// interface's name (`getifaddrs`).
pub fn addresses() -> Vec<(String, IpAddr)> {
    let mut out = vec![];
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below, once.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut p = head;
    while !p.is_null() {
        // SAFETY: `p` is a node of the list getifaddrs gave, alive until freeifaddrs.
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() || ifa.ifa_flags & libc::IFF_UP as libc::c_uint == 0 {
            continue;
        }
        // SAFETY: `ifa_addr` is a sockaddr whose family says which one it is.
        let ip = unsafe {
            match (*ifa.ifa_addr).sa_family as libc::c_int {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                }
                _ => continue,
            }
        };
        // SAFETY: `ifa_name` is a NUL-terminated C string.
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        out.push((name, ip));
    }
    // SAFETY: the list from getifaddrs above, freed once.
    unsafe { libc::freeifaddrs(head) };
    out
}

/// The interface that holds `ip`, among these addresses.
pub fn interface_among(addrs: &[(String, IpAddr)], ip: IpAddr) -> Option<String> {
    addrs.iter().find(|(_, a)| *a == ip).map(|(n, _)| n.clone())
}

/// The interface of this Mac that holds `ip` now, with its index: where an
/// advert for a listener on `ip` goes. `None`: no interface has it (Wi-Fi
/// is off, the Mac moved on).
pub fn interface_of(ip: IpAddr) -> Option<Interface> {
    let name = interface_among(&addresses(), ip)?;
    let c = std::ffi::CString::new(name.clone()).ok()?;
    // SAFETY: a NUL-terminated name; if_nametoindex only reads it.
    let index = unsafe { libc::if_nametoindex(c.as_ptr()) };
    (index != 0).then_some(Interface { name, index })
}

/// Where the phone API's listener on `ip` is advertised: its interface, but
/// never a loopback address's (no phone is on that network).
pub fn advert_interface(ip: IpAddr) -> Option<Interface> {
    if ip.is_loopback() {
        return None;
    }
    interface_of(ip)
}

/// This Mac's name, as Sharing in System Settings shows it ("Hassan's
/// MacBook Pro"); else its host name without `.local`.
pub fn computer_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some(n) = ffi::computer_name().filter(|n| !n.trim().is_empty()) {
        return Some(n);
    }
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let host = String::from_utf8_lossy(&buf[..end]).into_owned();
    let host = host.strip_suffix(".local").unwrap_or(&host).trim().to_string();
    (!host.is_empty()).then_some(host)
}

/// What mDNSResponder said of a registration.
#[derive(Debug, Default)]
struct Reg {
    /// The name asked for.
    asked: String,
    /// The name it is registered under (another one after a clash: "Mac (2)").
    name: Option<String>,
    /// It failed (mDNSResponder refused it, or went away): why.
    failed: Option<String>,
}

/// Shared with the thread that reads mDNSResponder's replies (and its callback).
struct Shared {
    reg: Mutex<Reg>,
    /// Called on each reply: someone may want to say the new name.
    on_reply: Box<dyn Fn() + Send + Sync>,
}

/// A Bonjour registration, withdrawn when dropped.
pub struct Advert {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    /// A test's advert: called when it is dropped.
    #[cfg(test)]
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Advert {
    /// Registers `name` as a `SERVICE` in `DOMAIN` on `interface`, at
    /// `port`, with this TXT record. On a name clash mDNSResponder picks
    /// another ("Mac (2)": `name()` says it), and `on_reply` is called
    /// (from another thread) whenever mDNSResponder says something.
    pub fn start(port: u16, interface: &Interface, name: &str, txt: &[u8], on_reply: impl Fn() + Send + Sync + 'static) -> Result<Advert> {
        let shared =
            Arc::new(Shared { reg: Mutex::new(Reg { asked: name.to_string(), ..Default::default() }), on_reply: Box::new(on_reply) });
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "macos")]
        let thread = Some(ffi::register(port, interface.index, name, txt, &shared, &stop)?);
        #[cfg(not(target_os = "macos"))]
        let thread: Option<thread::JoinHandle<()>> = {
            let _ = (port, interface, txt);
            anyhow::bail!("Bonjour is only advertised on macOS");
        };
        Ok(Advert {
            shared,
            stop,
            thread,
            #[cfg(test)]
            on_drop: None,
        })
    }

    /// A stand-in, registered as `name` at once; `on_drop` is called when it is withdrawn (tests).
    #[cfg(test)]
    pub fn fake(name: &str, on_drop: impl FnOnce() + Send + 'static) -> Advert {
        let reg = Reg { asked: name.to_string(), name: Some(name.to_string()), failed: None };
        Advert {
            shared: Arc::new(Shared { reg: Mutex::new(reg), on_reply: Box::new(|| {}) }),
            stop: Arc::default(),
            thread: None,
            on_drop: Some(Box::new(on_drop)),
        }
    }

    fn reg(&self) -> std::sync::MutexGuard<'_, Reg> {
        self.shared.reg.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The name it is found by: the one mDNSResponder registered (renamed
    /// on a clash), the one asked until it has said; `None` once it failed.
    pub fn name(&self) -> Option<String> {
        let r = self.reg();
        if r.failed.is_some() { None } else { Some(r.name.clone().unwrap_or_else(|| r.asked.clone())) }
    }

    /// Why it isn't registered any more (mDNSResponder refused it or went away).
    pub fn failed(&self) -> Option<String> {
        self.reg().failed.clone()
    }
}

impl Drop for Advert {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The thread deallocates the registration (which withdraws it) before it ends.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        #[cfg(test)]
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

/// An error from mDNSResponder in words (the ones that happen).
fn describe(err: i32) -> String {
    let what = match err {
        -65540 => "bad parameter",
        -65548 => "name conflict",
        -65563 => "mDNSResponder isn't running",
        -65569 => "policy denied",
        -65570 => "timeout",
        _ => "error",
    };
    format!("{what}, {err}")
}

#[cfg(target_os = "macos")]
mod ffi {
    //! `dns_sd.h` (libSystem) and `SCDynamicStoreCopyComputerName` (SystemConfiguration).

    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use anyhow::{Context, Result, bail};

    use super::{DOMAIN, SERVICE, Shared, describe};

    #[repr(C)]
    struct DNSServiceRefT {
        _private: [u8; 0],
    }
    type DNSServiceRef = *mut DNSServiceRefT;
    type DNSServiceFlags = u32;
    type DNSServiceErrorType = i32;
    type DNSServiceRegisterReply = extern "C" fn(
        sd_ref: DNSServiceRef,
        flags: DNSServiceFlags,
        error: DNSServiceErrorType,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        context: *mut c_void,
    );

    /// `kDNSServiceFlagsAdd`: in a register reply, the name is registered (else withdrawn).
    const FLAGS_ADD: DNSServiceFlags = 0x2;
    /// `kDNSServiceErr_NoError`.
    const NO_ERROR: DNSServiceErrorType = 0;
    /// How often the reader looks at its stop flag while mDNSResponder says nothing.
    const POLL_MS: c_int = 100;

    unsafe extern "C" {
        fn DNSServiceRegister(
            sd_ref: *mut DNSServiceRef,
            flags: DNSServiceFlags,
            interface_index: u32,
            name: *const c_char,
            regtype: *const c_char,
            domain: *const c_char,
            host: *const c_char,
            port: u16,
            txt_len: u16,
            txt_record: *const c_void,
            callback: Option<DNSServiceRegisterReply>,
            context: *mut c_void,
        ) -> DNSServiceErrorType;
        fn DNSServiceRefSockFD(sd_ref: DNSServiceRef) -> c_int;
        fn DNSServiceProcessResult(sd_ref: DNSServiceRef) -> DNSServiceErrorType;
        fn DNSServiceRefDeallocate(sd_ref: DNSServiceRef);
    }

    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCopyComputerName(store: *const c_void, encoding: *mut u32) -> core_foundation::string::CFStringRef;
    }

    /// The Computer Name (System Settings > General > Sharing).
    pub fn computer_name() -> Option<String> {
        use core_foundation::base::TCFType as _;
        // SAFETY: NULL store (a temporary session) and no encoding wanted; a +1 CFString or NULL.
        let s = unsafe { SCDynamicStoreCopyComputerName(std::ptr::null(), std::ptr::null_mut()) };
        if s.is_null() {
            return None;
        }
        // SAFETY: the copy above is ours to release (the create rule).
        Some(unsafe { core_foundation::string::CFString::wrap_under_create_rule(s) }.to_string())
    }

    /// The registration, owned by the reader thread: deallocated (so
    /// withdrawn) when that thread ends, or when it couldn't start.
    struct Sd(DNSServiceRef);

    // SAFETY: only the reader thread uses the ref once it is made (never two threads at once).
    unsafe impl Send for Sd {}

    impl Drop for Sd {
        fn drop(&mut self) {
            // SAFETY: a ref DNSServiceRegister initialised, deallocated once, by its only user.
            unsafe { DNSServiceRefDeallocate(self.0) };
        }
    }

    /// mDNSResponder's answer to a registration: the name taken, or why not.
    extern "C" fn registered(
        _sd_ref: DNSServiceRef,
        flags: DNSServiceFlags,
        error: DNSServiceErrorType,
        name: *const c_char,
        _regtype: *const c_char,
        _domain: *const c_char,
        context: *mut c_void,
    ) {
        // SAFETY: the context is the `Shared` the reader thread holds while it processes replies.
        let shared = unsafe { &*(context as *const Shared) };
        let name = (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned());
        {
            let mut r = shared.reg.lock().unwrap_or_else(|e| e.into_inner());
            match (error, name) {
                (NO_ERROR, Some(name)) if flags & FLAGS_ADD != 0 => {
                    if name != r.asked {
                        eprintln!("[phone] Bonjour: {:?} is taken on this network; found as {name:?} instead", r.asked);
                    } else if r.name.is_none() {
                        eprintln!("[phone] Bonjour: found as {name:?} ({SERVICE} in {DOMAIN})");
                    }
                    r.name = Some(name);
                }
                // Registered, then withdrawn by mDNSResponder: it renames and registers again by itself.
                (NO_ERROR, _) => eprintln!("[phone] Bonjour: {:?} was withdrawn on this network", r.name.as_deref().unwrap_or(&r.asked)),
                (e, _) => {
                    eprintln!("[phone] Bonjour: not advertised ({})", describe(e));
                    r.failed = Some(format!("mDNSResponder: {}", describe(e)));
                }
            }
        }
        (shared.on_reply)();
    }

    /// Registers, and starts the thread that reads the replies (and
    /// withdraws the registration when `stop` is set).
    pub fn register(
        port: u16,
        index: u32,
        name: &str,
        txt: &[u8],
        shared: &Arc<Shared>,
        stop: &Arc<AtomicBool>,
    ) -> Result<thread::JoinHandle<()>> {
        let c_name = CString::new(name).context("a name with a NUL in it")?;
        let (regtype, domain) = (CString::new(SERVICE)?, CString::new(DOMAIN)?);
        let txt_len = u16::try_from(txt.len()).context("a TXT record over 64 KiB")?;
        let mut sd: DNSServiceRef = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call (DNS-SD copies them); the context is
        // the `Shared` the reader thread keeps alive for as long as replies are processed.
        let err = unsafe {
            DNSServiceRegister(
                &mut sd,
                // 0: on a clash mDNSResponder renames it ("Mac (2)").
                0,
                // The interface that holds the listener's address (if_nametoindex).
                index,
                c_name.as_ptr(),
                regtype.as_ptr(),
                domain.as_ptr(),
                // This Mac's own host name: its address on that interface.
                std::ptr::null(),
                // Network byte order.
                port.to_be(),
                txt_len,
                txt.as_ptr() as *const c_void,
                Some(registered),
                Arc::as_ptr(shared) as *mut c_void,
            )
        };
        if err != NO_ERROR || sd.is_null() {
            bail!("DNSServiceRegister: {}", describe(err));
        }
        let sd = Sd(sd);
        // SAFETY: a ref just initialised.
        let fd = unsafe { DNSServiceRefSockFD(sd.0) };
        if fd < 0 {
            bail!("DNSServiceRefSockFD gave no socket");
        }
        let (shared, stop) = (shared.clone(), stop.clone());
        let t = thread::Builder::new().name("lan-bonjour".into()).spawn(move || {
            let sd = sd;
            while !stop.load(Ordering::SeqCst) {
                let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: one pollfd, ours.
                let n = unsafe { libc::poll(&mut p, 1, POLL_MS) };
                if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if n <= 0 {
                    if n < 0 {
                        break;
                    }
                    continue;
                }
                // SAFETY: the ref is this thread's alone; the socket has a reply (or closed).
                let err = unsafe { DNSServiceProcessResult(sd.0) };
                if err != NO_ERROR {
                    eprintln!("[phone] Bonjour: mDNSResponder's connection ended ({})", describe(err));
                    shared.reg.lock().unwrap_or_else(|e| e.into_inner()).failed = Some(format!("mDNSResponder: {}", describe(err)));
                    (shared.on_reply)();
                    break;
                }
            }
            // `sd` is deallocated here, after the last reply was processed: the advert is withdrawn.
            drop(sd);
            // (`shared` outlives every callback: it is dropped only after the ref.)
            drop(shared);
        })?;
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "8f3a1c0e5b7d9f21a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0";

    #[test]
    fn the_fingerprint_is_64_lowercase_hex_digits_however_it_was_written() {
        assert_eq!(fp_hex(FP).as_deref(), Some(FP));
        let upper_colons: String =
            FP.to_uppercase().as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(":");
        assert_eq!(fp_hex(&upper_colons).as_deref(), Some(FP), "AB:CD:…");
        assert_eq!(fp_hex(&crate::daemon::lan::tls::grouped(FP)).as_deref(), Some(FP), "grouped by four, as the Mac shows it");
        assert_eq!(fp_hex(&FP[..62]), None, "too short");
        assert_eq!(fp_hex(&format!("{}zz", &FP[..62])), None, "not hex");
    }

    #[test]
    fn the_txt_record_is_v_then_fp_then_name_each_at_most_255_bytes() {
        let t = txt(&FP.to_uppercase(), "Hassan’s MacBook Pro").unwrap();
        let entries = txt_entries(&t).unwrap();
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["v", "fp", "name"]);
        assert_eq!(entries[0].1, "1");
        assert_eq!(entries[1].1, FP, "normalised");
        assert_eq!(entries[2].1, "Hassan’s MacBook Pro");
        // Byte for byte: a length, then `key=value`.
        assert_eq!(t[0], 3);
        assert_eq!(&t[1..4], b"v=1");
        assert_eq!(t[4] as usize, 3 + 64);
        assert_eq!(&t[5..8], b"fp=");
        // A long name is cut at a character, so the string stays within 255 bytes.
        let long = "é".repeat(200);
        let t = txt(FP, &long).unwrap();
        let mut rest = &t[..];
        while let Some((&n, tail)) = rest.split_first() {
            assert!(n as usize <= MAX_TXT_STRING && tail.len() >= n as usize);
            rest = &tail[n as usize..];
        }
        let name = &txt_entries(&t).unwrap()[2].1;
        assert_eq!(name.len(), 250, "255 less `name=`, on a character's edge");
        assert!(name.chars().all(|c| c == 'é'));
        assert!(txt("not a fingerprint", "Mac").is_err());
    }

    #[test]
    fn the_instance_name_is_one_dns_label_and_never_empty() {
        assert_eq!(instance_name("  Hassan’s MacBook Pro \n"), "Hassan’s MacBook Pro");
        assert_eq!(instance_name(""), FALLBACK_NAME);
        assert_eq!(instance_name("\u{7}"), FALLBACK_NAME);
        let n = instance_name(&"ü".repeat(40));
        assert!(n.len() <= MAX_NAME && n.len() == 62, "{} bytes", n.len());
    }

    #[test]
    fn an_address_is_advertised_on_the_interface_that_holds_it() {
        let addrs = vec![
            ("lo0".to_string(), "127.0.0.1".parse().unwrap()),
            ("lo0".to_string(), "::1".parse().unwrap()),
            ("en0".to_string(), "fe80::1c2b:3d4e:5f60:7182".parse().unwrap()),
            ("en0".to_string(), "192.168.2.211".parse().unwrap()),
            ("utun4".to_string(), "100.68.110.33".parse().unwrap()),
            ("en7".to_string(), "10.10.30.116".parse().unwrap()),
        ];
        let on = |ip: &str| interface_among(&addrs, ip.parse().unwrap());
        assert_eq!(on("192.168.2.211").as_deref(), Some("en0"));
        assert_eq!(on("10.10.30.116").as_deref(), Some("en7"));
        assert_eq!(on("100.68.110.33").as_deref(), Some("utun4"));
        assert_eq!(on("192.168.2.99"), None, "an address this Mac doesn't have (Wi-Fi off, another network)");
        // This Mac, really: loopback is lo0, with its index.
        let lo = interface_of("127.0.0.1".parse().unwrap()).expect("127.0.0.1 is on an interface");
        assert!(lo.name.starts_with("lo") && lo.index > 0, "{lo:?}");
        assert!(addresses().iter().any(|(n, ip)| *n == lo.name && ip.is_loopback()));
        assert_eq!(interface_of("192.0.2.123".parse().unwrap()), None, "TEST-NET-1 is nobody's");
        // A loopback listener is never advertised: no phone is on that network.
        assert_eq!(advert_interface("127.0.0.1".parse().unwrap()), None);
    }

    #[test]
    fn this_mac_has_a_name() {
        let n = computer_name().expect("a computer name or a host name");
        assert!(!n.trim().is_empty() && !n.ends_with(".local"), "{n:?}");
    }

    #[test]
    fn a_fake_advert_says_its_name_and_when_it_is_withdrawn() {
        let gone = Arc::new(AtomicBool::new(false));
        let g = gone.clone();
        let a = Advert::fake("Desk Mac (2)", move || g.store(true, Ordering::SeqCst));
        assert_eq!((a.name().as_deref(), a.failed()), (Some("Desk Mac (2)"), None));
        drop(a);
        assert!(gone.load(Ordering::SeqCst));
    }
}
